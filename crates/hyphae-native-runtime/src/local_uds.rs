// SPDX-License-Identifier: Apache-2.0

use std::{
    fs, io,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
};

use crate::local_protocol::{DecodedFrame, FrameKind, LocalFrameIo, LocalTransportError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EndpointIdentity {
    device: u64,
    inode: u64,
}

impl EndpointIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_socket() && self == Self::from_metadata(metadata)
    }
}

/// Filesystem-owned Unix-domain listener for native local frames.
#[derive(Debug)]
pub struct UdsFrameListener {
    listener: UnixListener,
    path: PathBuf,
    identity: EndpointIdentity,
    maximum_payload: usize,
    closed: bool,
}

impl UdsFrameListener {
    /// Binds one filesystem-backed UDS endpoint without replacing any path.
    ///
    /// The caller owns the parent directory and its permissions. The socket
    /// itself is restricted to owner read/write.
    ///
    /// # Errors
    ///
    /// Returns an error for an existing endpoint, invalid payload bound,
    /// filesystem failure, bind failure, or permission failure.
    pub fn bind(
        path: impl AsRef<Path>,
        maximum_payload: usize,
    ) -> Result<Self, LocalTransportError> {
        let path = path.as_ref();
        reject_existing_endpoint(path)?;
        LocalFrameIo::new(maximum_payload)?;
        let listener = UnixListener::bind(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(LocalTransportError::EndpointReplaced);
        }
        let mut bound = Self {
            listener,
            path: path.to_owned(),
            identity: EndpointIdentity::from_metadata(&metadata),
            maximum_payload,
            closed: false,
        };
        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            let _ignored = bound.cleanup_endpoint();
            return Err(error.into());
        }
        Ok(bound)
    }

    /// Accepts one blocking ordered framed connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system rejects the accept or the
    /// configured framed-I/O state cannot be constructed.
    pub fn accept(&self) -> Result<UdsFrameConnection, LocalTransportError> {
        let (stream, _peer_address) = self.listener.accept()?;
        UdsFrameConnection::from_stream(stream, self.maximum_payload)
    }

    /// Returns the exact filesystem endpoint owned by this listener.
    pub fn local_path(&self) -> &Path {
        &self.path
    }

    /// Closes the listener and removes its unchanged socket endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the path was replaced or safe removal fails.
    pub fn close(mut self) -> Result<(), LocalTransportError> {
        self.cleanup_endpoint()
    }

    fn cleanup_endpoint(&mut self) -> Result<(), LocalTransportError> {
        if self.closed {
            return Ok(());
        }
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.closed = true;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if !self.identity.matches(&metadata) {
            return Err(LocalTransportError::EndpointReplaced);
        }
        fs::remove_file(&self.path)?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for UdsFrameListener {
    fn drop(&mut self) {
        let _ignored = self.cleanup_endpoint();
    }
}

/// One blocking Unix-domain connection carrying ordered native local frames.
#[derive(Debug)]
pub struct UdsFrameConnection {
    stream: UnixStream,
    frame_io: LocalFrameIo,
}

impl UdsFrameConnection {
    /// Connects to one filesystem-backed native local UDS endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid payload bound or connection failure.
    pub fn connect(
        path: impl AsRef<Path>,
        maximum_payload: usize,
    ) -> Result<Self, LocalTransportError> {
        let frame_io = LocalFrameIo::new(maximum_payload)?;
        let stream = UnixStream::connect(path)?;
        Ok(Self { stream, frame_io })
    }

    fn from_stream(
        stream: UnixStream,
        maximum_payload: usize,
    ) -> Result<Self, LocalTransportError> {
        Ok(Self {
            stream,
            frame_io: LocalFrameIo::new(maximum_payload)?,
        })
    }

    /// Returns the strict per-frame payload bound for this connection.
    pub const fn maximum_payload(&self) -> usize {
        self.frame_io.maximum_payload()
    }

    /// Sends one complete native local frame.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized payload or stream write failure.
    pub fn send(
        &mut self,
        kind: FrameKind,
        stream_id: u32,
        request_id: u64,
        payload: &[u8],
    ) -> Result<(), LocalTransportError> {
        self.frame_io
            .send_to(&mut self.stream, kind, stream_id, request_id, payload)
    }

    /// Receives one complete native local frame or clean stream EOF.
    ///
    /// The returned payload borrows this connection's reusable receive
    /// buffer and remains valid until the next mutable connection operation.
    ///
    /// # Errors
    ///
    /// Returns an error for truncation, configured-bound violation, protocol
    /// failure, or stream read failure.
    pub fn receive(&mut self) -> Result<Option<DecodedFrame<'_>>, LocalTransportError> {
        self.frame_io.receive_from(&mut self.stream)
    }
}

fn reject_existing_endpoint(path: &Path) -> Result<(), LocalTransportError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(LocalTransportError::EndpointExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
