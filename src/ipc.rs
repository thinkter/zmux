//! Platform-neutral local IPC transport used by zmux's child-process protocol.
//!
//! The Zed `net` crate deliberately exposes the same Unix-domain socket API on
//! every supported platform. On Windows its implementation uses Winsock
//! `AF_UNIX`; because zmux places every endpoint in a private per-user
//! directory, filesystem ACLs scope access to the current user. This gives the
//! notification protocol a local, capability-style endpoint without a TCP
//! listener or a fixed global name.

use std::{
    io::{self, Read, Write},
    path::Path,
};

/// Common interface that keeps the notification protocol independent of the
/// OS-specific local-socket implementation.
pub(crate) trait LocalIpcTransport {
    type Listener: Send + 'static;
    type Stream: Read + Write + Send + 'static;

    fn bind(endpoint: &Path) -> io::Result<Self::Listener>;
    fn accept(listener: &Self::Listener) -> io::Result<Self::Stream>;
    fn connect(endpoint: &Path) -> io::Result<Self::Stream>;
}

/// The local transport supplied by Zed's portable `net` crate.
pub(crate) struct PlatformLocalIpc;

impl LocalIpcTransport for PlatformLocalIpc {
    type Listener = net::UnixListener;
    type Stream = net::UnixStream;

    fn bind(endpoint: &Path) -> io::Result<Self::Listener> {
        net::UnixListener::bind(endpoint)
    }

    fn accept(listener: &Self::Listener) -> io::Result<Self::Stream> {
        listener.accept().map(|(stream, _)| stream)
    }

    fn connect(endpoint: &Path) -> io::Result<Self::Stream> {
        net::UnixStream::connect(endpoint)
    }
}
