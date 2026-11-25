//! One directory per posted file. A private staging directory holds the
//! content and record until the core authorizes their publication.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};
use std::time::Duration;

use t7_proto::files::{
    Attachment, CHUNK_BYTES, ChannelFile, Cursor, FileId, MAX_LIMIT, PAGE_SIZE, Reply, SessionKey, Upload, valid_name,
};
use t7_proto::{ChannelId, ClientId, IDLE_TIMEOUT_SECS, Identity, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use tokio::time::timeout;

use crate::state;

const RECORD: &str = "record.toml";
const CONTENT: &str = "content";
const UNAVAILABLE: &str = "This file or channel is unavailable";
const IDLE: Duration = Duration::from_secs(IDLE_TIMEOUT_SECS);

pub fn new_id() -> io::Result<FileId> {
    let mut id = [0; 16];
    getrandom::fill(&mut id).map_err(|err| io::Error::other(err.to_string()))?;
    Ok(id)
}

pub fn new_key() -> io::Result<String> {
    new_id().map(|id| t7_proto::update::to_hex(&id))
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub channel_key: String,
    #[serde(default)]
    pub temporary_channel: bool,
    pub attachment: Attachment,
}

impl Record {
    fn for_channel(&self, channel: ChannelId) -> ChannelFile {
        ChannelFile { channel, attachment: self.attachment.clone() }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    max_file_size_bytes: u64,
}

pub struct Store {
    root: PathBuf,
    _temporary: Option<tempfile::TempDir>,
    settings: Option<PathBuf>,
    limit: u64,
    records: BTreeMap<Cursor, Record>,
    by_id: HashMap<FileId, Cursor>,
}

impl Store {
    pub fn open(data_dir: Option<&Path>, limit: u64, channels: HashSet<&str>) -> io::Result<Self> {
        let temporary = data_dir.is_none().then(tempfile::tempdir).transpose()?;
        let root = data_dir
            .map_or_else(|| temporary.as_ref().expect("temporary root").path().to_owned(), |path| path.join("files"));
        let settings = data_dir.map(|path| path.join("file-settings.toml"));
        let limit = match settings.as_deref().map(fs::read_to_string).transpose() {
            Ok(Some(text)) => toml::from_str::<Settings>(&text).map_err(io::Error::other)?.max_file_size_bytes,
            Ok(None) => limit,
            Err(e) if e.kind() == io::ErrorKind::NotFound => limit,
            Err(e) => return Err(e),
        };
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(io::Error::other("max_file_size_bytes is outside the supported range"));
        }
        fs::create_dir_all(&root)?;
        let mut store =
            Self { root, _temporary: temporary, settings, limit, records: BTreeMap::new(), by_id: HashMap::new() };
        for entry in fs::read_dir(&store.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let path = entry.path();
            if name.to_str().is_some_and(|name| name.starts_with(".upload-")) {
                remove_dir(&path);
                continue;
            }
            match load_record(&path, &name) {
                Ok(record) if channels.contains(record.channel_key.as_str()) => store.insert(record),
                Ok(record) if record.temporary_channel => remove_dir(&path),
                Ok(_) => tracing::warn!(path = %path.display(), "stored file belongs to a missing permanent channel"),
                Err(e) => tracing::warn!(path = %path.display(), "cannot read stored file: {e}"),
            }
        }
        Ok(store)
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn set_limit(&mut self, bytes: u64) -> io::Result<()> {
        if !(1..=MAX_LIMIT).contains(&bytes) {
            return Err(io::Error::other("Enter a positive file size within the supported range"));
        }
        if let Some(path) = &self.settings {
            let text = toml::to_string(&Settings { max_file_size_bytes: bytes }).map_err(io::Error::other)?;
            crate::write_atomic(path, |tmp| fs::write(tmp, &text))?;
        }
        self.limit = bytes;
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn get(&self, id: FileId) -> Option<&Record> {
        self.by_id.get(&id).and_then(|key| self.records.get(key))
    }

    pub fn content(&self, id: FileId) -> PathBuf {
        self.directory(id).join(CONTENT)
    }

    fn directory(&self, id: FileId) -> PathBuf {
        self.root.join(t7_proto::update::to_hex(&id))
    }

    pub fn publish(&mut self, record: Record, staged: tempfile::TempDir) -> io::Result<()> {
        let dest = self.directory(record.attachment.key.id);
        if dest.exists() {
            return Err(io::Error::other("file ID already exists"));
        }
        fs::rename(staged.path(), dest)?;
        self.insert(record);
        Ok(())
    }

    fn insert(&mut self, record: Record) {
        let key = record.attachment.key;
        self.by_id.insert(key.id, key);
        self.records.insert(key, record);
    }

    pub fn remove_channels(&mut self, channels: &HashSet<String>) {
        let mut removed = Vec::new();
        self.records.retain(|key, record| {
            let keep = !channels.contains(record.channel_key.as_str());
            if !keep {
                self.by_id.remove(&key.id);
                removed.push(key.id);
            }
            keep
        });
        if !removed.is_empty() {
            let dirs: Vec<_> = removed.into_iter().map(|id| self.directory(id)).collect();
            tokio::task::spawn_blocking(move || {
                for dir in dirs {
                    remove_dir(&dir);
                }
            });
        }
    }

    pub fn page(
        &self,
        channels: &HashMap<&str, (ChannelId, &str)>,
        before: Option<Cursor>,
        query: &str,
    ) -> (Vec<ChannelFile>, Option<Cursor>) {
        let query = query.trim().to_lowercase();
        let bounds = (Unbounded, before.map_or(Unbounded, Excluded));
        let mut matches = self.records.range(bounds).rev().filter_map(|(_, record)| {
            let &(channel, channel_name) = channels.get(record.channel_key.as_str())?;
            let attachment = &record.attachment;
            attachment.matches_query(channel_name, &query).then_some((record, channel))
        });
        let files: Vec<_> = matches.by_ref().take(PAGE_SIZE).map(|(r, channel)| r.for_channel(channel)).collect();
        let next = matches.next().and_then(|_| files.last().map(|f| f.attachment.key));
        (files, next)
    }
}

fn load_record(path: &Path, name: &std::ffi::OsStr) -> io::Result<Record> {
    let record: Record = toml::from_str(&fs::read_to_string(path.join(RECORD))?).map_err(io::Error::other)?;
    let attachment = &record.attachment;
    if name != std::ffi::OsStr::new(&t7_proto::update::to_hex(&attachment.key.id))
        || !valid_name(&attachment.name)
        || attachment.nickname.chars().count() > t7_proto::MAX_NICKNAME_LEN + 10
        || attachment.caption.chars().count() > t7_proto::MAX_TEXT_LEN
        || fs::metadata(path.join(CONTENT))?.len() != attachment.size
    {
        return Err(io::Error::other("invalid attachment record"));
    }
    Ok(record)
}

fn remove_dir(path: &Path) {
    if let Err(e) = fs::remove_dir_all(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), "cannot remove file directory: {e}");
    }
}

pub enum Operation {
    Upload { root: PathBuf, record: Record },
    Download { path: PathBuf, size: u64 },
}

pub struct Job {
    pub client: ClientId,
    pub channel: ChannelId,
    pub operation: Operation,
    pub permit: OwnedSemaphorePermit,
}

pub enum Core {
    Start {
        client: ClientId,
        key: SessionKey,
        identity: Option<Identity>,
        request: t7_proto::files::Request,
        reply: oneshot::Sender<Result<Job, String>>,
    },
    Check {
        client: ClientId,
        channel: ChannelId,
        posting: bool,
        reply: oneshot::Sender<bool>,
    },
    Publish {
        client: ClientId,
        channel: ChannelId,
        record: Record,
        staged: tempfile::TempDir,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

pub async fn transfer(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    core: &mpsc::Sender<state::Event>,
    client: ClientId,
    key: SessionKey,
    identity: Option<Identity>,
    request: t7_proto::files::Request,
) {
    let job = ask(core, |reply| Core::Start { client, key, identity, request, reply }).await;
    let result = match job {
        Ok(Ok(job)) => run(&mut stream, core, job).await,
        Ok(Err(reason)) | Err(reason) => Err(reason),
    };
    let reply = match result {
        Ok(()) => Reply::Done,
        Err(reason) => Reply::Error(reason),
    };
    let _ = timeout(IDLE, async {
        write_frame(&mut stream, &reply).await?;
        stream.shutdown().await?;
        Ok::<_, t7_proto::FrameError>(())
    })
    .await;
}

async fn run(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    core: &mpsc::Sender<state::Event>,
    job: Job,
) -> Result<(), String> {
    let Job { client, channel, operation, permit: _permit } = job;
    match operation {
        Operation::Upload { root, record } => receive(stream, core, client, channel, root, record).await,
        Operation::Download { path, size } => send(stream, core, client, channel, path, size).await,
    }
}

async fn receive(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    core: &mpsc::Sender<state::Event>,
    client: ClientId,
    channel: ChannelId,
    root: PathBuf,
    mut record: Record,
) -> Result<(), String> {
    let staged = tokio::task::spawn_blocking(move || tempfile::Builder::new().prefix(".upload-").tempdir_in(root))
        .await
        .map_err(storage_error)?
        .map_err(storage_error)?;
    let mut output = tokio::fs::File::create(staged.path().join(CONTENT)).await.map_err(storage_error)?;
    let size = record.attachment.size;
    respond(stream, &Reply::Ready { size }).await?;
    let mut received = 0;
    let mut scratch = Vec::new();
    loop {
        let upload: Upload = timeout(IDLE, read_frame(stream, &mut scratch))
            .await
            .map_err(|_| "Upload timed out".to_owned())?
            .map_err(|e| e.to_string())?;
        match upload {
            Upload::Chunk(bytes) => {
                if bytes.is_empty() || bytes.len() > CHUNK_BYTES || bytes.len() as u64 > size - received {
                    return Err("Upload exceeded its declared size or chunk size".into());
                }
                check(core, client, channel, true).await?;
                output.write_all(&bytes).await.map_err(storage_error)?;
                received += bytes.len() as u64;
            }
            Upload::Finish if received == size => break,
            Upload::Finish => return Err("Upload ended before the declared size".into()),
        }
    }
    let publish = async {
        output.flush().await.map_err(storage_error)?;
        output.sync_all().await.map_err(storage_error)?;
        drop(output);
        record.attachment.key.posted_ms = state::unix_ms();
        let text = toml::to_string(&record).map_err(storage_error)?;
        let mut metadata = tokio::fs::File::create(staged.path().join(RECORD)).await.map_err(storage_error)?;
        metadata.write_all(text.as_bytes()).await.map_err(storage_error)?;
        metadata.flush().await.map_err(storage_error)?;
        metadata.sync_all().await.map_err(storage_error)?;
        drop(metadata);
        ask(core, |reply| Core::Publish { client, channel, record, staged, reply }).await?
    };
    tokio::pin!(publish);
    loop {
        tokio::select! {
            result = &mut publish => return result,
            () = tokio::time::sleep(Duration::from_secs(5)) => respond(stream, &Reply::Finalizing).await?,
        }
    }
}

async fn send(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    core: &mpsc::Sender<state::Event>,
    client: ClientId,
    channel: ChannelId,
    path: PathBuf,
    size: u64,
) -> Result<(), String> {
    let mut input = tokio::fs::File::open(path).await.map_err(storage_error)?;
    check(core, client, channel, false).await?;
    respond(stream, &Reply::Ready { size }).await?;
    let mut remaining = size;
    while remaining > 0 {
        let mut bytes = vec![0; remaining.min(CHUNK_BYTES as u64) as usize];
        input.read_exact(&mut bytes).await.map_err(storage_error)?;
        check(core, client, channel, false).await?;
        remaining -= bytes.len() as u64;
        respond(stream, &Reply::Chunk(bytes)).await?;
    }
    check(core, client, channel, false).await
}

async fn respond(stream: &mut (impl AsyncWrite + Unpin), reply: &Reply) -> Result<(), String> {
    timeout(IDLE, write_frame(stream, reply))
        .await
        .map_err(|_| "Transfer timed out".to_owned())?
        .map_err(|e| e.to_string())
}

async fn ask<T>(
    core: &mpsc::Sender<state::Event>,
    request: impl FnOnce(oneshot::Sender<T>) -> Core,
) -> Result<T, String> {
    let (tx, rx) = oneshot::channel();
    core.send(state::Event::File(request(tx))).await.map_err(|_| "Server disconnected".to_owned())?;
    rx.await.map_err(|_| "Server disconnected".to_owned())
}

async fn check(
    core: &mpsc::Sender<state::Event>,
    client: ClientId,
    channel: ChannelId,
    posting: bool,
) -> Result<(), String> {
    if ask(core, |reply| Core::Check { client, channel, posting, reply }).await? {
        Ok(())
    } else {
        Err(UNAVAILABLE.into())
    }
}

pub fn unavailable() -> String {
    UNAVAILABLE.into()
}

pub fn storage_error(error: impl std::fmt::Display) -> String {
    tracing::warn!("file storage: {error}");
    "The server could not read or write the file".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_channels_hide_permanent_files_and_clean_up_temporary_rooms() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(Some(dir.path()), 1, HashSet::new()).unwrap();
        for (id, temporary_channel) in [(1, false), (2, true)] {
            let attachment = Attachment {
                key: Cursor { posted_ms: 0, id: [id; 16] },
                author: Identity([0; 32]),
                nickname: "Sam".into(),
                name: "notes.txt".into(),
                size: 1,
                caption: String::new(),
            };
            let record = Record { channel_key: "original".into(), temporary_channel, attachment };
            let staged = tempfile::tempdir_in(store.root()).unwrap();
            fs::write(staged.path().join(CONTENT), [42]).unwrap();
            fs::write(staged.path().join(RECORD), toml::to_string(&record).unwrap()).unwrap();
            store.publish(record, staged).unwrap();
        }
        drop(store);
        let missing = Store::open(Some(dir.path()), 1, HashSet::new()).unwrap();
        assert!(missing.get([1; 16]).is_none());
        assert!(missing.content([1; 16]).exists());
        assert!(!missing.directory([2; 16]).exists());
        let restored = Store::open(Some(dir.path()), 1, HashSet::from(["original"])).unwrap();
        assert_eq!(restored.get([1; 16]).unwrap().attachment.name, "notes.txt");
    }
}
