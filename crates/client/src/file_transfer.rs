//! Background attachment I/O. Each task uses a separate pinned TLS socket.

use std::io::Cursor;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use t7_proto::files::{CHUNK_BYTES, FileId, Reply, Request, SessionKey, Upload};
use t7_proto::identity::IdentityKey;
use t7_proto::tls::{self, Fingerprint};
use t7_proto::{C2S, ChannelId, ClientId, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::timeout;

const IDLE: Duration = Duration::from_secs(25);
const ACTIVE: u8 = 0;
const CANCELED: u8 = 1;
const FINALIZING: u8 = 2;

pub struct Connection {
    pub address: SocketAddr,
    pub fingerprint: Fingerprint,
    pub client: ClientId,
    pub key: SessionKey,
    pub identity: IdentityKey,
}

pub struct Task {
    pub id: u64,
    pub operation: Operation,
    pub cancel: Cancellation,
}

pub enum Operation {
    Upload { channel: ChannelId, source: Source, name: String, size: u64, caption: String },
    Download { file: FileId, size: u64, target: Download },
}

pub enum Source {
    Disk(PathBuf),
    Clipboard(Vec<u8>),
}

pub enum Download {
    Save(PathBuf),
    Preview,
}

pub enum Update {
    Progress { bytes: u64, finalizing: bool },
    Finished(Result<Outcome, String>),
}

pub enum Outcome {
    Uploaded,
    Saved,
    Preview(egui::ColorImage),
    Canceled,
}

pub struct Cancel {
    state: Arc<AtomicU8>,
    signal: watch::Sender<bool>,
}

impl Cancel {
    pub fn request(&self) -> bool {
        if self.state.compare_exchange(ACTIVE, CANCELED, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _ = self.signal.send(true);
            true
        } else {
            false
        }
    }
}

impl Drop for Cancel {
    fn drop(&mut self) {
        self.request();
    }
}

pub struct Cancellation {
    state: Arc<AtomicU8>,
    signal: watch::Receiver<bool>,
}

pub fn cancellation() -> (Cancel, Cancellation) {
    let (tx, rx) = watch::channel(false);
    let state = Arc::new(AtomicU8::new(ACTIVE));
    (Cancel { state: state.clone(), signal: tx }, Cancellation { state, signal: rx })
}

struct Progress<F> {
    emit: F,
    last: Instant,
    state: Arc<AtomicU8>,
}

impl<F: Fn(Update)> Progress<F> {
    fn update(&mut self, bytes: u64) {
        if self.last.elapsed() >= Duration::from_millis(100) {
            self.last = Instant::now();
            (self.emit)(Update::Progress { bytes, finalizing: false });
        }
    }

    fn finalize(&self, bytes: u64) -> Result<(), String> {
        self.state
            .compare_exchange(ACTIVE, FINALIZING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "Transfer canceled".to_owned())?;
        (self.emit)(Update::Progress { bytes, finalizing: true });
        Ok(())
    }
}

pub async fn run(connection: Option<Connection>, task: Task, emit: impl Fn(Update) + Send + Sync) {
    let Task { operation, cancel: Cancellation { state, mut signal }, .. } = task;
    let mut progress = Progress { emit: &emit, last: Instant::now(), state };
    let result = if let Some(connection) = connection {
        tokio::select! {
            biased;
            _ = signal.wait_for(|canceled| *canceled) => Ok(Outcome::Canceled),
            result = run_transfer(connection, operation, &mut progress) => result,
        }
    } else {
        Err("Files are unavailable on this connection".into())
    };
    emit(Update::Finished(if progress.state.load(Ordering::Acquire) == CANCELED {
        Ok(Outcome::Canceled)
    } else {
        result
    }));
}

async fn run_transfer(
    connection: Connection,
    operation: Operation,
    progress: &mut Progress<impl Fn(Update)>,
) -> Result<Outcome, String> {
    let request = match &operation {
        Operation::Upload { channel, name, size, caption, .. } => {
            Request::Upload { channel: *channel, name: name.clone(), size: *size, caption: caption.clone() }
        }
        Operation::Download { file, .. } => Request::Download { id: *file },
    };
    let mut stream = timeout(Duration::from_secs(8), async {
        let stream = TcpStream::connect(connection.address).await.map_err(message)?;
        let (mut tls, _) = tls::connect(stream, Some(connection.fingerprint)).await.map_err(message)?;
        let identity = connection.identity.prove(&tls::exporter(tls.get_ref().1));
        write_frame(&mut tls, &C2S::FileHello { client: connection.client, key: connection.key, identity, request })
            .await
            .map_err(message)?;
        Ok::<_, String>(tls)
    })
    .await
    .map_err(|_| "The file connection timed out".to_owned())??;
    let mut scratch = Vec::new();
    let Reply::Ready { size: accepted } = next(&mut stream, &mut scratch).await? else {
        return Err("The server did not start the file transfer".into());
    };
    match operation {
        Operation::Upload { source, size, .. } if size == accepted => {
            match source {
                Source::Disk(path) => {
                    let file = tokio::fs::File::open(path).await.map_err(message)?;
                    if file.metadata().await.map_err(message)?.len() != size {
                        return Err("The selected file changed size before uploading".into());
                    }
                    upload(stream, file, size, progress).await?;
                }
                Source::Clipboard(bytes) => upload(stream, Cursor::new(bytes), size, progress).await?,
            }
            Ok(Outcome::Uploaded)
        }
        Operation::Download { size, target, .. } if size == accepted => match target {
            Download::Save(path) => {
                let dir = path.parent().ok_or("Choose a folder for the downloaded file")?;
                let partial = tempfile::Builder::new().prefix(".t7-download-").tempfile_in(dir).map_err(message)?;
                let mut file = tokio::fs::File::from_std(partial.reopen().map_err(message)?);
                download(&mut stream, &mut file, &mut scratch, size, progress).await?;
                file.sync_all().await.map_err(message)?;
                drop(file);
                progress.finalize(size)?;
                partial.persist(path).map_err(message)?;
                Ok(Outcome::Saved)
            }
            Download::Preview if size <= crate::image_preview::MAX_ENCODED => {
                let mut bytes = Vec::with_capacity(size as usize);
                download(&mut stream, &mut bytes, &mut scratch, size, progress).await?;
                let image = tokio::task::spawn_blocking(move || crate::image_preview::decode(&bytes))
                    .await
                    .map_err(message)??;
                Ok(Outcome::Preview(image))
            }
            Download::Preview => Err("Image is too large to preview".into()),
        },
        _ => Err("The file size has changed".into()),
    }
}

async fn upload(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    mut input: impl AsyncRead + Unpin,
    size: u64,
    progress: &mut Progress<impl Fn(Update)>,
) -> Result<(), String> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut scratch = Vec::new();
    let mut response = {
        let response = read_frame::<_, Reply>(&mut reader, &mut scratch);
        tokio::pin!(response);
        let sending = async {
            let mut sent = 0;
            while sent < size {
                let mut chunk = vec![0; (size - sent).min(CHUNK_BYTES as u64) as usize];
                input.read_exact(&mut chunk).await.map_err(message)?;
                sent += chunk.len() as u64;
                send(&mut writer, &Upload::Chunk(chunk)).await?;
                progress.update(sent);
            }
            match input.read_u8().await {
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {}
                Ok(_) => return Err("The selected file grew during uploading".into()),
                Err(err) => return Err(message(err)),
            }
            progress.finalize(size)?;
            send(&mut writer, &Upload::Finish).await
        };
        tokio::select! {
            biased;
            reply = &mut response => reply.map_err(message)?,
            result = sending => {
                result?;
                timeout(IDLE, response).await.map_err(|_| "The upload timed out".to_owned())?.map_err(message)?
            }
        }
    };
    while matches!(response, Reply::Finalizing) {
        let mut scratch = Vec::new();
        response = next(&mut reader, &mut scratch).await?;
    }
    match response {
        Reply::Done => Ok(()),
        Reply::Error(reason) => Err(reason),
        _ => Err("Invalid upload response".into()),
    }
}

async fn download(
    stream: &mut (impl AsyncRead + Unpin),
    output: &mut (impl AsyncWrite + Unpin),
    scratch: &mut Vec<u8>,
    size: u64,
    progress: &mut Progress<impl Fn(Update)>,
) -> Result<(), String> {
    let mut bytes = 0;
    loop {
        match next(stream, scratch).await? {
            Reply::Chunk(chunk)
                if !chunk.is_empty() && chunk.len() <= CHUNK_BYTES && chunk.len() as u64 <= size - bytes =>
            {
                output.write_all(&chunk).await.map_err(message)?;
                bytes += chunk.len() as u64;
                progress.update(bytes);
            }
            Reply::Done if bytes == size => return output.flush().await.map_err(message),
            _ => return Err("The server sent an incomplete or invalid file".into()),
        }
    }
}

async fn next(stream: &mut (impl AsyncRead + Unpin), scratch: &mut Vec<u8>) -> Result<Reply, String> {
    match timeout(IDLE, read_frame(stream, scratch))
        .await
        .map_err(|_| "The download timed out".to_owned())?
        .map_err(message)?
    {
        Reply::Error(reason) => Err(reason),
        reply => Ok(reply),
    }
}

async fn send(stream: &mut (impl AsyncWrite + Unpin), upload: &Upload) -> Result<(), String> {
    timeout(IDLE, write_frame(stream, upload)).await.map_err(|_| "The upload timed out".to_owned())?.map_err(message)
}

fn message(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::sync::Mutex;
    use t7_proto::files::{Command, Event, GB};
    use t7_proto::{PROTOCOL_VERSION, S2C};
    use tokio::net::TcpListener;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "streams more than 4 GiB in each direction; run in release with 14 GB of temporary disk space"]
    async fn large_file_round_trip() {
        let size = (1u64 << 32) + 257;
        let scratch = tempfile::tempdir().unwrap();
        let source = scratch.path().join("recording.bin");
        let dest = scratch.path().join("saved.bin");
        let mut input = std::fs::File::create(&source).unwrap();
        input.set_len(size).unwrap();
        for offset in [0, GB, 2 * GB, 3 * GB, 4 * GB, 1 << 32, size - 8] {
            input.seek(SeekFrom::Start(offset)).unwrap();
            input.write_all(&offset.to_le_bytes()).unwrap();
        }
        input.sync_all().unwrap();
        drop(input);

        let dir = scratch.path().join("server");
        std::fs::create_dir(&dir).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let config = t7_server::config::Config { max_file_size_bytes: 5 * GB, ..Default::default() };
            t7_server::serve(listener, &config, Some(&dir)).await.unwrap();
        });
        let (mut control, fingerprint) = tls::connect(TcpStream::connect(address).await.unwrap(), None).await.unwrap();
        let identity = IdentityKey::from_seed([7; 32]);
        let signed = identity.prove(&tls::exporter(control.get_ref().1));
        write_frame(
            &mut control,
            &C2S::Hello {
                protocol: PROTOCOL_VERSION,
                nickname: "Transfer test".into(),
                password: None,
                identity: signed,
            },
        )
        .await
        .unwrap();
        let mut scratch_buf = Vec::new();
        let S2C::Welcome { your_id: client, clients, .. } = read_frame(&mut control, &mut scratch_buf).await.unwrap()
        else {
            panic!("server refused connection")
        };
        let channel = clients.iter().find(|c| c.id == client).unwrap().channel;
        write_frame(&mut control, &C2S::Files(Command::Subscribe)).await.unwrap();
        let key = loop {
            if let S2C::Files(Event::Ready { limit, key }) = read_frame(&mut control, &mut scratch_buf).await.unwrap() {
                assert_eq!(limit, 5 * GB);
                break key;
            }
        };
        let (mut reader, mut writer) = tokio::io::split(control);
        let keepalive = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(4)).await;
                write_frame(&mut writer, &C2S::Ping { nonce: 1 }).await.unwrap();
            }
        });
        let connection = || Connection { address, client, fingerprint, key, identity: identity.clone() };
        let transfer = |operation| {
            let conn = connection();
            async move {
                let (_handle, cancel) = cancellation();
                let result = Mutex::new(None);
                run(Some(conn), Task { id: 1, operation, cancel }, |update| {
                    if let Update::Finished(outcome) = update {
                        *result.lock().unwrap() = Some(outcome.unwrap());
                    }
                })
                .await;
                result.into_inner().unwrap().unwrap()
            }
        };
        assert!(matches!(
            transfer(Operation::Upload {
                channel,
                source: Source::Disk(source.clone()),
                name: "recording.bin".into(),
                size,
                caption: "Full transfer".into()
            })
            .await,
            Outcome::Uploaded
        ));
        let file = loop {
            if let S2C::Files(Event::Posted(file)) = read_frame(&mut reader, &mut scratch_buf).await.unwrap() {
                break file;
            }
        };
        assert_eq!(file.attachment.size, size);
        assert!(matches!(
            transfer(Operation::Download { file: file.attachment.key.id, size, target: Download::Save(dest.clone()) })
                .await,
            Outcome::Saved
        ));
        keepalive.abort();
        server.abort();
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), size);
        let mut expected = std::fs::File::open(source).unwrap();
        let mut actual = std::fs::File::open(dest).unwrap();
        let mut a = vec![0; CHUNK_BYTES];
        let mut b = vec![0; CHUNK_BYTES];
        loop {
            let n = expected.read(&mut a).unwrap();
            if n == 0 {
                break;
            }
            actual.read_exact(&mut b[..n]).unwrap();
            assert!(a[..n] == b[..n], "saved bytes differ from the source");
        }
    }
}
