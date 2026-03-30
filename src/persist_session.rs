use std::path::{Path, PathBuf};

use dialoguer::{Input, Password};
use matrix_sdk::{Client, authentication::matrix::MatrixSession};
use rand::{RngExt, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{error, info};

/// The data needed to rebuild the client
#[derive(Debug, Serialize, Deserialize)]
struct ClientSession {
    /// The URL to the homeserver of the user
    homeserver: String,
    /// The path of the database
    db_path: PathBuf,
    /// The passphrase of the database
    passphrase: String,
}

/// The full session to persist
#[derive(Debug, Serialize, Deserialize)]
struct FullSession {
    /// The data to re-build the client
    client_session: ClientSession,
    /// The matrix user session
    user_session: MatrixSession,
    /// The latest sync token
    ///
    /// It is only needed to persist it when using `Client::sync_once()` and we
    /// want to make our syncs faster by not receiving all the initial sync
    /// again.
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
}

pub async fn restore_session(session_file: &Path) -> anyhow::Result<(Client, Option<String>)> {
    info!("Previous session found in {}", session_file.display());

    // The session was serialized as JSON in a file.
    let serialized_session = fs::read_to_string(session_file).await?;
    let FullSession {
        client_session,
        user_session,
        sync_token,
    } = serde_json::from_str(&serialized_session)?;

    // Build the client with the previous settings from the session.
    let client = Client::builder()
        .homeserver_url(client_session.homeserver)
        .sqlite_store(client_session.db_path, Some(&client_session.passphrase))
        .build()
        .await?;

    info!("Restoring session for {}…", user_session.meta.user_id);

    // Restore the Matrix user session.
    client.restore_session(user_session).await?;

    Ok((client, sync_token))
}

/// Login with a new device.
pub async fn login(data_dir: &Path, session_file: &Path) -> anyhow::Result<Client> {
    info!("No previous session found, logging in…");

    let (client, client_session) = build_client(data_dir).await?;
    let matrix_auth = client.matrix_auth();

    loop {
        let username: String = Input::new().with_prompt("Your username").interact_text().unwrap();
        let username = username.trim();

        let password: String = Password::new().with_prompt("Your password").interact().unwrap();
        let password = password.trim();

        match matrix_auth
            .login_username(username, password)
            .initial_device_display_name("matrix-crous-bot client")
            .await
        {
            Ok(_) => {
                info!("Logged in as {username}");
                break;
            }
            Err(error) => {
                error!("Error logging in: {error}");
                error!("Please try again");
            }
        }
    }

    // Persist the session to reuse it later.
    // This is not very secure, for simplicity. If the system provides a way of
    // storing secrets securely, it should be used instead.
    // Note that we could also build the user session from the login response.
    let user_session = matrix_auth
        .session()
        .expect("A logged-in client should have a session");
    let serialized_session = serde_json::to_string(&FullSession {
        client_session,
        user_session,
        sync_token: None,
    })?;
    fs::write(session_file, serialized_session).await?;

    info!("Session persisted in {}", session_file.to_string_lossy());

    // After logging in, you might want to verify this session with another one (see
    // the `emoji_verification` example), or bootstrap cross-signing if this is your
    // first session with encryption, or if you need to reset cross-signing because
    // you don't have access to your old sessions (see the
    // `cross_signing_bootstrap` example).

    Ok(client)
}

/// Build a new client.
async fn build_client(data_dir: &Path) -> anyhow::Result<(Client, ClientSession)> {
    let mut rng = rand::rng();

    // Generating a subfolder for the database is not mandatory, but it is useful if
    // you allow several clients to run at the same time. Each one must have a
    // separate database, which is a different folder with the SQLite store.
    let db_subfolder: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let db_path = data_dir.join(db_subfolder);

    // Generate a random passphrase.
    let passphrase: String = (&mut rng)
        .sample_iter(Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    // We create a loop here so the user can retry if an error happens.
    loop {
        // let mut homeserver = String::new();

        let homeserver: String = Input::new().with_prompt("Homeserver URL").interact().unwrap();

        info!("Checking homeserver…");

        match Client::builder()
            .homeserver_url(&homeserver)
            // We use the SQLite store, which is enabled by default. This is the crucial part to
            // persist the encryption setup.
            // Note that other store backends are available and you can even implement your own.
            .sqlite_store(&db_path, Some(&passphrase))
            .build()
            .await
        {
            Ok(client) => {
                return Ok((
                    client,
                    ClientSession {
                        homeserver,
                        db_path,
                        passphrase,
                    },
                ));
            }
            Err(error) => match &error {
                matrix_sdk::ClientBuildError::AutoDiscovery(_)
                | matrix_sdk::ClientBuildError::Url(_)
                | matrix_sdk::ClientBuildError::Http(_) => {
                    error!("Error checking the homeserver: {error}");
                    error!("Please try again");
                }
                _ => {
                    // Forward other errors, it's unlikely we can retry with a different outcome.
                    return Err(error.into());
                }
            },
        }
    }
}

/// Persist the sync token for a future session.
/// Note that this is needed only when using `sync_once`. Other sync methods get
/// the sync token from the store.
pub async fn persist_sync_token(session_file: &Path, sync_token: String) -> anyhow::Result<()> {
    let serialized_session = fs::read_to_string(session_file).await?;
    let mut full_session: FullSession = serde_json::from_str(&serialized_session)?;

    full_session.sync_token = Some(sync_token);
    let serialized_session = serde_json::to_string(&full_session)?;
    fs::write(session_file, serialized_session).await?;

    Ok(())
}
