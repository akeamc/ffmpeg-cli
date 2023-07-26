use std::{path::PathBuf, process::Stdio, ffi::{OsStr, OsString}};

use futures_lite::future::zip;
use tokio::{net::{UnixListener, UnixStream}, io::{AsyncRead, AsyncWrite, BufReader, AsyncBufReadExt, Sink, Empty}, process::Command, sync::mpsc};

struct TempSocket {
  dir: tempfile::TempDir,
  path: PathBuf,
  listener: UnixListener,
}

impl std::ops::Deref for TempSocket {
  type Target = UnixListener;

  fn deref(&self) -> &Self::Target {
      &self.listener
  }
}

impl TempSocket {
  pub fn new() -> std::io::Result<Self> {
      let dir = tempfile::tempdir()?;
      let path = dir.path().join("sock");
      let listener = UnixListener::bind(&path)?;
      Ok(Self {
          dir,
          path,
          listener,
      })
  }
}

pub enum Input<'a, R> {
  File(PathBuf),
  Stream(&'a mut R),
}

impl Input<'_, Empty> {
  fn file(path: impl Into<PathBuf>) -> Self {
      Self::File(path.into())
  }
}

pub enum Output<'a, W> {
  File(PathBuf),
  Stream(&'a mut W),
}

impl Output<'_, Sink> {
  fn file(path: impl Into<PathBuf>) -> Self {
      Self::File(path.into())
  }
}

pub struct FfmpegBuilder<'a, R, W>
where
  R: AsyncRead + Unpin,
  W: AsyncWrite + Unpin,
{
  pub global_options: Vec<OsString>,
  pub input_options: Vec<OsString>,
  pub input: Input<'a, R>,
  pub output_options: Vec<OsString>,
  pub output: Output<'a, W>,
}

impl<'a, R, W> FfmpegBuilder<'a, R, W>
where
  R: AsyncRead + Unpin,
  W: AsyncWrite + Unpin,
{
  fn to_command(&self, progress_url: impl AsRef<OsStr>) -> Command {
      // ffmpeg [global_options] {[input_file_options] -i input_url} ... {[output_file_options] output_url} ...

      let mut cmd = Command::new("ffmpeg");

      cmd.arg("-progress");
      cmd.arg(progress_url);
      cmd.args(&self.global_options);

      cmd.args(&self.input_options);
      cmd.arg("-i");
      match &self.input {
          Input::File(path) => {
              cmd.arg(path);
          }
          Input::Stream(_) => {
              cmd.arg("pipe:0"); // stdin
              cmd.stdin(Stdio::piped());
          }
      }

      cmd.args(&self.output_options);
      match &self.output {
          Output::File(path) => {
              cmd.arg(path);
          }
          Output::Stream(_) => {
              cmd.arg("pipe:1"); // stdout
              cmd.stdout(Stdio::piped());
          }
      }

      cmd
  }

  pub fn spawn(self) -> anyhow::Result<Ffmpeg<'a, R, W>> {
      let progress_sock = TempSocket::new()?;
      let mut cmd = self.to_command(format!("unix://{}", progress_sock.path.to_str().unwrap()));
      let child = cmd.spawn()?;

      let (mut progress_tx, progress_rx) = mpsc::unbounded_channel();
      tokio::spawn(async move {
          let (mut stream, _) = progress_sock.accept().await.unwrap();
          read_progress(&mut stream, &mut progress_tx).await;
      });

      let Self { input, output, .. } = self;

      Ok(Ffmpeg {
          child,
          input,
          output,
          progress_rx,
      })
  }
}

async fn read_progress(stream: &mut UnixStream, tx: &mut mpsc::UnboundedSender<Progress>) {
  let mut lines = BufReader::new(stream).lines();

  let mut progress = Progress::default();

  while let Some(line) = lines.next_line().await.unwrap() {
      if let Some((k, v)) = line.split_once('=') {
          match k {
              "total_size" => {
                  progress.total_size = Some(v.parse().unwrap());
              },
              "progress" => {
                  dbg!(progress);
                  progress = Progress::default();
              }
              _ => {
                  println!("unknown progress key: {}", k);
              },
          }
          println!("{}: {}", k, v);
      }
  }
}

pub struct Ffmpeg<'a, R, W>
where
  R: AsyncRead + Unpin,
  W: AsyncWrite + Unpin,
{
  child: tokio::process::Child,
  input: Input<'a, R>,
  output: Output<'a, W>,
  progress_rx: mpsc::UnboundedReceiver<Progress>,
}

#[derive(Debug, Default)]
struct Progress {
  total_size: Option<u64>,
}

impl<R, W> Ffmpeg<'_, R, W>
where
  R: AsyncRead + Unpin,
  W: AsyncWrite + Unpin,
{
  pub async fn wait(&mut self) -> anyhow::Result<()> {
      let mut stdout = self.child.stdout.take().unwrap();
      let mut stdin = self.child.stdin.take().unwrap();

      let stdout = async {
          match self.output {
              Output::File(_) => std::io::Result::Ok(()),
              Output::Stream(ref mut write) => {
                  tokio::io::copy(&mut stdout, write).await?;
                  drop(stdout); // drop to close stdout
                  Ok(())
              }
          }
      };
      let stdin = async {
          match self.input {
              Input::File(_) => std::io::Result::Ok(()),
              Input::Stream(ref mut read) => {
                  tokio::io::copy(read, &mut stdin).await?;
                  drop(stdin); // drop to close stdin
                  Ok(())
              }
          }
      };

      tokio::select! {
          _ = self.child.wait() => {},
          (read, write) = zip(stdout, stdin) => {
              read?;
              write?;
          }
      }

      if !self.child.wait().await?.success() {
          panic!("ffmpeg failed");
      }

      Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn test() {
      let mut ffmpeg = FfmpegBuilder {
          global_options: vec![],
          input_options: vec![],
          input: Input::file(PathBuf::from("video.webm")),
          output_options: vec![],
          output: Output::file(PathBuf::from("test2.mp3")),
      }
      .spawn()
      .unwrap();

      ffmpeg.wait().await.unwrap();
  }
}
