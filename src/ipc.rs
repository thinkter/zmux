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
    type Stream: Read + Write + Send + 'static;

    fn connect(endpoint: &Path) -> io::Result<Self::Stream>;
}

/// The local transport supplied by Zed's portable `net` crate.
pub(crate) struct PlatformLocalIpc;

impl LocalIpcTransport for PlatformLocalIpc {
    type Stream = net::UnixStream;

    fn connect(endpoint: &Path) -> io::Result<Self::Stream> {
        net::UnixStream::connect(endpoint)
    }
}
