pub mod json_log;
pub mod protocol;
pub mod proxy;
pub mod proxy_http;
pub mod remote_client;
pub mod remote_identity;
mod transport;

pub use proxy_http::ProxyHttpRelay;
#[cfg(any(test, feature = "test-support"))]
pub use remote_client::DisplayLanguageForTests;
#[cfg(target_os = "windows")]
pub use remote_client::OpenWslPath;
pub use remote_client::{
    CommandTemplate, ConnectionIdentifier, ConnectionState, Interactive, RemoteArch, RemoteClient,
    RemoteClientDelegate, RemoteClientEvent, RemoteConnection, RemoteConnectionOptions, RemoteOs,
    RemotePlatform, connect, has_active_connection, set_client_instance_id,
};
pub use remote_client::{DISPLAY_LANGUAGE_ENV_VAR, DisplayLanguageHandler};
pub use remote_identity::{
    RemoteConnectionIdentity, remote_connection_identity, same_remote_connection_identity,
};
pub use transport::docker::DockerConnectionOptions;
pub use transport::ssh::{
    SshConnectionOptions, SshPortForwardOption, take_remote_connection_warnings,
};
pub use transport::wsl::WslConnectionOptions;
#[cfg(target_os = "windows")]
pub use transport::wsl::wsl_path_to_windows_path;

#[cfg(any(test, feature = "test-support"))]
pub use transport::mock::{
    MockConnection, MockConnectionOptions, MockConnectionRegistry, MockDelegate,
};
