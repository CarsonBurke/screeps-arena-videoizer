use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{Error, Rational, Result};

static OUTPUT_ID: AtomicU64 = AtomicU64::new(0);
static FFMPEG_REAPER: OnceLock<std::result::Result<mpsc::Sender<ReapRequest>, String>> =
    OnceLock::new();
const MAX_CAPTURED_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_FINISH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    H264Nvenc,
    H264Software,
}

impl VideoCodec {
    const fn encoder_name(self) -> &'static str {
        match self {
            Self::H264Nvenc => "h264_nvenc",
            Self::H264Software => "libx264",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: Rational,
    pub codec: VideoCodec,
    /// H.264 quantizer/CRF in FFmpeg's 0–51 range.
    pub quality: u8,
    pub overwrite: bool,
}

impl VideoEncoderConfig {
    pub fn nvenc(width: u32, height: u32, frames_per_second: Rational) -> Self {
        Self {
            width,
            height,
            frames_per_second,
            codec: VideoCodec::H264Nvenc,
            quality: 18,
            overwrite: false,
        }
    }

    pub fn frame_bytes(self) -> Result<usize> {
        validate_config(self)?;
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            .map(|bytes| bytes / 2)
            .ok_or(Error::ArithmeticOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoEncoderStats {
    pub frames: u64,
    pub bytes: u64,
}

/// Streams tightly packed NV12 frames to one long-lived FFmpeg process.
///
/// Keeping FFmpeg alive for the entire replay avoids per-frame process and
/// container overhead. The default NVENC configuration uses the lowest-latency
/// preset; callers can select the software codec for compatibility/testing.
#[must_use = "the encoder must be finished to flush and validate the video"]
pub struct FfmpegVideoEncoder {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stderr_reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    frame_bytes: usize,
    frames: u64,
    output: std::path::PathBuf,
    temporary_output: std::path::PathBuf,
    overwrite: bool,
    failed: bool,
}

/// Streams low-overhead AV1 OBUs produced by the direct NVENC path into an
/// FFmpeg stream-copy muxer. FFmpeg never decodes or re-encodes the frames; it
/// only builds the MP4 container and publishes it atomically on success.
#[must_use = "the muxer must be finished to flush and validate the video"]
pub struct FfmpegAv1Muxer {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stderr_reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
    output: std::path::PathBuf,
    temporary_output: std::path::PathBuf,
    overwrite: bool,
    bytes: u64,
    failed: bool,
}

struct ReapRequest {
    child: Child,
    stderr_reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
}

impl FfmpegVideoEncoder {
    pub fn spawn(output: impl AsRef<Path>, config: VideoEncoderConfig) -> Result<Self> {
        Self::spawn_with_program("ffmpeg", output, config)
    }

    fn spawn_with_program(
        program: impl AsRef<OsStr>,
        output: impl AsRef<Path>,
        config: VideoEncoderConfig,
    ) -> Result<Self> {
        validate_config(config)?;
        let output = output.as_ref();
        if output.as_os_str().is_empty() {
            return Err(Error::Invalid(
                "video output path cannot be empty".to_owned(),
            ));
        }
        if !config.overwrite && output.exists() {
            return Err(Error::Invalid(format!(
                "video output already exists: {}",
                output.display()
            )));
        }
        // Establish the long-lived cleanup worker before creating an FFmpeg
        // process. Every subsequent error path can therefore transfer process
        // ownership without depending on another thread being available.
        drop(reaper_sender()?);
        let temporary_output = temporary_output_path(output)?;
        let frame_bytes = config.frame_bytes()?;
        let dimensions = format!("{}x{}", config.width, config.height);
        let frame_rate = format!(
            "{}/{}",
            config.frames_per_second.numerator(),
            config.frames_per_second.denominator()
        );
        let mut command = Command::new(program);
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            if config.overwrite { "-y" } else { "-n" },
            "-f",
            "rawvideo",
            "-pixel_format",
            "nv12",
            "-video_size",
            &dimensions,
            "-framerate",
            &frame_rate,
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            config.codec.encoder_name(),
        ]);
        match config.codec {
            VideoCodec::H264Nvenc => {
                command.args([
                    "-preset",
                    "p1",
                    "-tune",
                    "ull",
                    "-rc",
                    "constqp",
                    "-qp",
                    &config.quality.to_string(),
                ]);
            }
            VideoCodec::H264Software => {
                command.args(["-preset", "ultrafast", "-crf", &config.quality.to_string()]);
            }
        }
        command.args([
            "-pix_fmt",
            "yuv420p",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-color_range",
            "tv",
            "-bsf:v",
            "h264_metadata=video_full_range_flag=0:colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1",
        ]);
        command.arg(&temporary_output);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_child(&mut child, None);
                let _ = fs::remove_file(&temporary_output);
                return Err(Error::Invalid(
                    "FFmpeg process did not expose a stdin pipe".to_owned(),
                ));
            }
        };
        let mut stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                drop(stdin);
                terminate_child(&mut child, None);
                let _ = fs::remove_file(&temporary_output);
                return Err(Error::Invalid(
                    "FFmpeg process did not expose a stderr pipe".to_owned(),
                ));
            }
        };
        let stderr_reader = match thread::Builder::new()
            .name("ffmpeg-stderr".to_owned())
            .spawn(move || {
                let mut bytes = Vec::new();
                let mut chunk = [0; 8 * 1024];
                loop {
                    let count = stderr.read(&mut chunk)?;
                    if count == 0 {
                        break;
                    }
                    let retained = (MAX_CAPTURED_STDERR_BYTES - bytes.len()).min(count);
                    bytes.extend_from_slice(&chunk[..retained]);
                }
                Ok(bytes)
            }) {
            Ok(reader) => reader,
            Err(error) => {
                drop(stdin);
                terminate_child(&mut child, None);
                let _ = fs::remove_file(&temporary_output);
                return Err(Error::Io(error));
            }
        };
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stderr_reader: Some(stderr_reader),
            frame_bytes,
            frames: 0,
            output: output.to_owned(),
            temporary_output,
            overwrite: config.overwrite,
            failed: false,
        })
    }

    pub const fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    pub fn write_frame(&mut self, nv12: &[u8]) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid(
                "video encoder is in a terminal failed state".to_owned(),
            ));
        }
        if nv12.len() != self.frame_bytes {
            return Err(Error::Invalid(format!(
                "NV12 frame has {} bytes; expected {}",
                nv12.len(),
                self.frame_bytes
            )));
        }
        let write_result = write_all_with_timeout(
            self.stdin
                .as_mut()
                .ok_or_else(|| Error::Invalid("video encoder is already finished".to_owned()))?,
            nv12,
            DEFAULT_WRITE_TIMEOUT,
        );
        if let Err(error) = write_result {
            drop(self.stdin.take());
            if let Some(child) = self.child.take() {
                defer_terminate_child(child, self.stderr_reader.take());
            }
            let _ = fs::remove_file(&self.temporary_output);
            self.failed = true;
            return Err(Error::Io(error));
        }
        self.frames = self
            .frames
            .checked_add(1)
            .ok_or(Error::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<VideoEncoderStats> {
        if self.failed {
            return Err(Error::Invalid(
                "video encoder is in a terminal failed state".to_owned(),
            ));
        }
        drop(self.stdin.take());
        let child = self
            .child
            .as_mut()
            .expect("live encoder retains its FFmpeg process");
        let status = match wait_child_with_timeout(child, DEFAULT_FINISH_TIMEOUT) {
            Ok(Some(status)) => status,
            Ok(None) => {
                if let Some(child) = self.child.take() {
                    defer_terminate_child(child, self.stderr_reader.take());
                }
                let _ = fs::remove_file(&self.temporary_output);
                self.failed = true;
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out flushing and muxing the FFmpeg output",
                )));
            }
            Err(error) => {
                if let Some(child) = self.child.take() {
                    defer_terminate_child(child, self.stderr_reader.take());
                }
                let _ = fs::remove_file(&self.temporary_output);
                self.failed = true;
                return Err(Error::Io(error));
            }
        };
        drop(self.child.take());
        let stderr = self.join_stderr()?;
        if !status.success() {
            let _ = fs::remove_file(&self.temporary_output);
            let message = String::from_utf8_lossy(&stderr);
            let message = message.trim();
            return Err(Error::Invalid(if message.is_empty() {
                format!("FFmpeg exited with status {status}")
            } else {
                format!("FFmpeg exited with status {status}: {message}")
            }));
        }
        if self.frames == 0 {
            let _ = fs::remove_file(&self.temporary_output);
            return Err(Error::Invalid(
                "video encoder received no frames".to_owned(),
            ));
        }
        let bytes = self
            .frames
            .checked_mul(self.frame_bytes as u64)
            .ok_or(Error::ArithmeticOverflow)?;
        publish_output(&self.temporary_output, &self.output, self.overwrite)?;
        Ok(VideoEncoderStats {
            frames: self.frames,
            bytes,
        })
    }

    fn join_stderr(&mut self) -> Result<Vec<u8>> {
        self.stderr_reader
            .take()
            .ok_or_else(|| Error::Invalid("FFmpeg stderr reader is unavailable".to_owned()))?
            .join()
            .map_err(|_| Error::Invalid("FFmpeg stderr reader panicked".to_owned()))?
            .map_err(Error::from)
    }
}

impl Drop for FfmpegVideoEncoder {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(child) = self.child.take() {
            defer_terminate_child(child, self.stderr_reader.take());
        } else if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        let _ = fs::remove_file(&self.temporary_output);
    }
}

impl FfmpegAv1Muxer {
    pub fn spawn(
        output: impl AsRef<Path>,
        frames_per_second: Rational,
        overwrite: bool,
    ) -> Result<Self> {
        Self::spawn_with_program("ffmpeg", output, frames_per_second, overwrite)
    }

    fn spawn_with_program(
        program: impl AsRef<OsStr>,
        output: impl AsRef<Path>,
        frames_per_second: Rational,
        overwrite: bool,
    ) -> Result<Self> {
        if frames_per_second == Rational::ZERO {
            return Err(Error::Invalid(
                "video frame rate must be positive".to_owned(),
            ));
        }
        let output = output.as_ref();
        if output.as_os_str().is_empty() {
            return Err(Error::Invalid(
                "video output path cannot be empty".to_owned(),
            ));
        }
        if !overwrite && output.exists() {
            return Err(Error::Invalid(format!(
                "video output already exists: {}",
                output.display()
            )));
        }
        drop(reaper_sender()?);
        let temporary_output = temporary_output_path(output)?;
        let frame_rate = format!(
            "{}/{}",
            frames_per_second.numerator(),
            frames_per_second.denominator()
        );
        let mut command = Command::new(program);
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            if overwrite { "-y" } else { "-n" },
            "-framerate",
            &frame_rate,
            "-f",
            "obu",
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            "copy",
            "-color_range",
            "tv",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-movflags",
            "+faststart",
        ]);
        command
            .arg(&temporary_output)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_child(&mut child, None);
                let _ = fs::remove_file(&temporary_output);
                return Err(Error::Invalid(
                    "FFmpeg process did not expose a stdin pipe".to_owned(),
                ));
            }
        };
        let mut stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                drop(stdin);
                terminate_child(&mut child, None);
                let _ = fs::remove_file(&temporary_output);
                return Err(Error::Invalid(
                    "FFmpeg process did not expose a stderr pipe".to_owned(),
                ));
            }
        };
        let stderr_reader = match thread::Builder::new()
            .name("ffmpeg-av1-stderr".to_owned())
            .spawn(move || {
                let mut bytes = Vec::new();
                let mut chunk = [0; 8 * 1024];
                loop {
                    let count = stderr.read(&mut chunk)?;
                    if count == 0 {
                        break;
                    }
                    let retained = (MAX_CAPTURED_STDERR_BYTES - bytes.len()).min(count);
                    bytes.extend_from_slice(&chunk[..retained]);
                }
                Ok(bytes)
            }) {
            Ok(reader) => reader,
            Err(error) => {
                drop(stdin);
                terminate_child(&mut child, None);
                let _ = fs::remove_file(&temporary_output);
                return Err(Error::Io(error));
            }
        };
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stderr_reader: Some(stderr_reader),
            output: output.to_owned(),
            temporary_output,
            overwrite,
            bytes: 0,
            failed: false,
        })
    }

    pub fn write_packet(&mut self, obu: &[u8]) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid(
                "AV1 muxer is in a terminal failed state".to_owned(),
            ));
        }
        if obu.is_empty() {
            return Err(Error::Invalid("AV1 packet cannot be empty".to_owned()));
        }
        let write_result = write_all_with_timeout(
            self.stdin
                .as_mut()
                .ok_or_else(|| Error::Invalid("AV1 muxer is already finished".to_owned()))?,
            obu,
            DEFAULT_WRITE_TIMEOUT,
        );
        if let Err(error) = write_result {
            drop(self.stdin.take());
            if let Some(child) = self.child.take() {
                defer_terminate_child(child, self.stderr_reader.take());
            }
            let _ = fs::remove_file(&self.temporary_output);
            self.failed = true;
            return Err(Error::Io(error));
        }
        self.bytes = self
            .bytes
            .checked_add(obu.len() as u64)
            .ok_or(Error::ArithmeticOverflow)?;
        Ok(())
    }

    pub fn finish(mut self, frames: u64) -> Result<VideoEncoderStats> {
        if self.failed {
            return Err(Error::Invalid(
                "AV1 muxer is in a terminal failed state".to_owned(),
            ));
        }
        if frames == 0 || self.bytes == 0 {
            return Err(Error::Invalid("AV1 muxer received no frames".to_owned()));
        }
        drop(self.stdin.take());
        let child = self
            .child
            .as_mut()
            .expect("live AV1 muxer retains its FFmpeg process");
        let status = match wait_child_with_timeout(child, DEFAULT_FINISH_TIMEOUT) {
            Ok(Some(status)) => status,
            Ok(None) => {
                if let Some(child) = self.child.take() {
                    defer_terminate_child(child, self.stderr_reader.take());
                }
                let _ = fs::remove_file(&self.temporary_output);
                self.failed = true;
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out flushing the AV1 MP4 muxer",
                )));
            }
            Err(error) => {
                if let Some(child) = self.child.take() {
                    defer_terminate_child(child, self.stderr_reader.take());
                }
                let _ = fs::remove_file(&self.temporary_output);
                self.failed = true;
                return Err(Error::Io(error));
            }
        };
        drop(self.child.take());
        let stderr = self
            .stderr_reader
            .take()
            .ok_or_else(|| Error::Invalid("FFmpeg stderr reader is unavailable".to_owned()))?
            .join()
            .map_err(|_| Error::Invalid("FFmpeg stderr reader panicked".to_owned()))??;
        if !status.success() {
            let _ = fs::remove_file(&self.temporary_output);
            let message = String::from_utf8_lossy(&stderr);
            let message = message.trim();
            return Err(Error::Invalid(if message.is_empty() {
                format!("FFmpeg exited with status {status}")
            } else {
                format!("FFmpeg exited with status {status}: {message}")
            }));
        }
        publish_output(&self.temporary_output, &self.output, self.overwrite)?;
        Ok(VideoEncoderStats {
            frames,
            bytes: self.bytes,
        })
    }
}

impl Drop for FfmpegAv1Muxer {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(child) = self.child.take() {
            defer_terminate_child(child, self.stderr_reader.take());
        } else if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
        let _ = fs::remove_file(&self.temporary_output);
    }
}

fn defer_terminate_child(
    child: Child,
    stderr_reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
) {
    let request = ReapRequest {
        child,
        stderr_reader,
    };
    match reaper_sender() {
        Ok(sender) => {
            if let Err(error) = sender.send(request) {
                let mut request = error.0;
                terminate_child(&mut request.child, request.stderr_reader.take());
            }
        }
        Err(_) => {
            // Encoders preflight the reaper before spawning, so this can only
            // happen if that process-wide invariant is broken unexpectedly.
            let mut request = request;
            terminate_child(&mut request.child, request.stderr_reader.take());
        }
    }
}

fn reaper_sender() -> Result<mpsc::Sender<ReapRequest>> {
    match FFMPEG_REAPER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<ReapRequest>();
        thread::Builder::new()
            .name("ffmpeg-reaper".to_owned())
            .spawn(move || {
                while let Ok(mut request) = receiver.recv() {
                    terminate_child(&mut request.child, request.stderr_reader.take());
                }
            })
            .map(|_| sender)
            .map_err(|error| error.to_string())
    }) {
        Ok(sender) => Ok(sender.clone()),
        Err(message) => Err(Error::Invalid(format!(
            "failed to start FFmpeg cleanup worker: {message}"
        ))),
    }
}

fn terminate_child(child: &mut Child, stderr_reader: Option<JoinHandle<std::io::Result<Vec<u8>>>>) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    if let Some(reader) = stderr_reader {
        let _ = reader.join();
    }
}

fn wait_child_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "timeout overflow"))?;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

#[cfg(unix)]
fn write_all_with_timeout(
    pipe: &mut ChildStdin,
    mut bytes: &[u8],
    timeout: Duration,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let fd = pipe.as_raw_fd();
    // Preserve the descriptor flags so callers do not observe an accidental
    // blocking-mode change if a write fails.
    let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if original_flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, original_flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let restore = PipeFlagsGuard { fd, original_flags };
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "timeout overflow"))?;
    while !bytes.is_empty() {
        match pipe.write(bytes) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write NV12 frame to FFmpeg",
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                wait_until_writable(fd, deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
    drop(restore);
    Ok(())
}

#[cfg(unix)]
struct PipeFlagsGuard {
    fd: std::os::fd::RawFd,
    original_flags: libc::c_int,
}

#[cfg(unix)]
impl Drop for PipeFlagsGuard {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(self.fd, libc::F_SETFL, self.original_flags);
        }
    }
}

#[cfg(unix)]
fn wait_until_writable(fd: std::os::fd::RawFd, deadline: Instant) -> std::io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out writing an NV12 frame to FFmpeg",
            ));
        }
        let milliseconds = remaining.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if result > 0 {
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "FFmpeg input pipe closed",
                ));
            }
            if descriptor.revents & libc::POLLOUT != 0 {
                return Ok(());
            }
        } else if result == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out writing an NV12 frame to FFmpeg",
            ));
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(not(unix))]
fn write_all_with_timeout(
    pipe: &mut ChildStdin,
    bytes: &[u8],
    _timeout: Duration,
) -> std::io::Result<()> {
    pipe.write_all(bytes)
}

fn temporary_output_path(output: &Path) -> Result<std::path::PathBuf> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = output
        .file_stem()
        .ok_or_else(|| Error::Invalid("video output path must include a file name".to_owned()))?;
    let extension = output.extension().ok_or_else(|| {
        Error::Invalid("video output path must include a container extension".to_owned())
    })?;
    let id = OUTPUT_ID.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(stem);
    name.push(format!(".{}.{id}.tmp.", std::process::id()));
    name.push(extension);
    Ok(parent.join(name))
}

fn publish_output(temporary_output: &Path, output: &Path, overwrite: bool) -> Result<()> {
    if overwrite {
        fs::rename(temporary_output, output)?;
        return Ok(());
    }
    // A same-directory hard link publishes the complete inode atomically and
    // fails instead of clobbering a path that appeared after preflight.
    fs::hard_link(temporary_output, output).map_err(|error| {
        let _ = fs::remove_file(temporary_output);
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Invalid(format!(
                "video output appeared while encoding: {}",
                output.display()
            ))
        } else {
            Error::Io(error)
        }
    })?;
    // The complete output is already atomically published. A stale hidden
    // link is cleanup debt, not an encoding failure.
    let _ = fs::remove_file(temporary_output);
    Ok(())
}

fn validate_config(config: VideoEncoderConfig) -> Result<()> {
    if config.width == 0 || config.height == 0 {
        return Err(Error::Invalid(
            "video dimensions must be positive".to_owned(),
        ));
    }
    if !config.width.is_multiple_of(2) || !config.height.is_multiple_of(2) {
        return Err(Error::Invalid(
            "NV12 video dimensions must be even".to_owned(),
        ));
    }
    if config.quality > 51 {
        return Err(Error::Invalid(
            "H.264 quality must be in the 0–51 range".to_owned(),
        ));
    }
    if config.frames_per_second == Rational::ZERO {
        return Err(Error::Invalid(
            "video frame rate must be positive".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use super::{
        FfmpegAv1Muxer, FfmpegVideoEncoder, VideoCodec, VideoEncoderConfig,
        wait_child_with_timeout, write_all_with_timeout,
    };
    use crate::{Rational, rgba8_to_nv12_reference};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn config() -> VideoEncoderConfig {
        VideoEncoderConfig {
            width: 4,
            height: 2,
            frames_per_second: Rational::new(60_000, 1_001).unwrap(),
            codec: VideoCodec::H264Software,
            quality: 18,
            overwrite: false,
        }
    }

    #[test]
    fn validates_nv12_extent_and_frame_size() {
        assert_eq!(config().frame_bytes().unwrap(), 12);
        let mut invalid = config();
        invalid.width = 3;
        assert!(invalid.frame_bytes().is_err());
        invalid.width = 4;
        invalid.quality = 52;
        assert!(invalid.frame_bytes().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pipe_write_times_out_when_the_encoder_stops_consuming() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let error = write_all_with_timeout(
            &mut stdin,
            &vec![0_u8; 1024 * 1024],
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(stdin);
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn process_wait_times_out_during_a_stalled_mux_flush() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(
            wait_child_with_timeout(&mut child, Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn streams_exact_frames_through_one_encoder_process() {
        let unique = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "screeps-arena-video-encoder-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let program = directory.join("fake-ffmpeg");
        let output = directory.join("output.bin");
        fs::write(
            &program,
            "#!/bin/sh\nfor last do :; done\ncat > \"$last\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&program, permissions).unwrap();

        let mut encoder =
            FfmpegVideoEncoder::spawn_with_program(&program, &output, config()).unwrap();
        assert_eq!(encoder.frame_bytes(), 12);
        encoder.write_frame(&[1; 12]).unwrap();
        assert!(encoder.write_frame(&[2; 11]).is_err());
        encoder.write_frame(&[3; 12]).unwrap();
        let stats = encoder.finish().unwrap();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.bytes, 24);
        assert_eq!(fs::read(&output).unwrap(), [[1; 12], [3; 12]].concat());

        let raced_output = directory.join("raced.bin");
        let mut raced =
            FfmpegVideoEncoder::spawn_with_program(&program, &raced_output, config()).unwrap();
        fs::write(&raced_output, b"new owner").unwrap();
        raced.write_frame(&[4; 12]).unwrap();
        assert!(raced.finish().is_err());
        assert_eq!(fs::read(&raced_output).unwrap(), b"new owner");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn streams_av1_packets_through_one_mux_process() {
        let unique = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "screeps-arena-av1-muxer-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let program = directory.join("fake-ffmpeg");
        let output = directory.join("output.mp4");
        fs::write(
            &program,
            "#!/bin/sh\nfor last do :; done\ncat > \"$last\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&program, permissions).unwrap();

        let mut muxer = FfmpegAv1Muxer::spawn_with_program(
            &program,
            &output,
            Rational::new(30, 1).unwrap(),
            false,
        )
        .unwrap();
        muxer.write_packet(&[1, 2, 3]).unwrap();
        assert!(muxer.write_packet(&[]).is_err());
        muxer.write_packet(&[4, 5]).unwrap();
        let stats = muxer.finish(2).unwrap();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.bytes, 5);
        assert_eq!(fs::read(&output).unwrap(), [1, 2, 3, 4, 5]);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn installed_ffmpeg_muxes_exact_nv12_frame_count() {
        let encoders = match Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
        {
            Ok(output) if output.status.success() => output,
            _ => return,
        };
        if !String::from_utf8_lossy(&encoders.stdout).contains("libx264")
            || Command::new("ffprobe").arg("-version").output().is_err()
        {
            return;
        }
        let unique = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "screeps-arena-real-video-encoder-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join("output.mp4");
        let mut real_config = VideoEncoderConfig {
            width: 16,
            height: 16,
            frames_per_second: Rational::new(60, 1).unwrap(),
            codec: VideoCodec::H264Software,
            quality: 0,
            overwrite: false,
        };
        let mut encoder = FfmpegVideoEncoder::spawn(&output, real_config).unwrap();
        let color = rgba8_to_nv12_reference(16, 16, &[32, 64, 96, 255].repeat(256)).unwrap();
        let white = rgba8_to_nv12_reference(16, 16, &[255, 255, 255, 255].repeat(256)).unwrap();
        encoder.write_frame(&color).unwrap();
        encoder.write_frame(&white).unwrap();
        assert_eq!(encoder.finish().unwrap().frames, 2);

        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height,avg_frame_rate,nb_read_frames,color_range,color_space,color_transfer,color_primaries",
                "-of",
                "json",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(probe.status.success());
        let probe: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        let stream = &probe["streams"][0];
        assert_eq!(stream["width"], 16);
        assert_eq!(stream["height"], 16);
        assert_eq!(stream["avg_frame_rate"], "60/1");
        assert_eq!(stream["nb_read_frames"], "2");
        assert_eq!(stream["color_range"], "tv");
        assert_eq!(stream["color_space"], "bt709");
        assert_eq!(stream["color_transfer"], "bt709");
        assert_eq!(stream["color_primaries"], "bt709");

        let decoded = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                output.to_str().unwrap(),
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "nv12",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(decoded.status.success());
        assert_eq!(decoded.stdout, color);

        real_config.overwrite = true;
        let mut overwrite = FfmpegVideoEncoder::spawn(&output, real_config).unwrap();
        overwrite.write_frame(&color).unwrap();
        assert_eq!(overwrite.finish().unwrap().frames, 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
