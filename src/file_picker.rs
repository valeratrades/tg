//! xdg-desktop-portal FileChooser, so the pick goes through whatever file manager the desktop has configured.
use std::{collections::HashMap, path::PathBuf};

use eyre::{Result, bail, eyre};
use futures_util::StreamExt as _;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[zbus::proxy(
	interface = "org.freedesktop.portal.FileChooser",
	default_service = "org.freedesktop.portal.Desktop",
	default_path = "/org/freedesktop/portal/desktop"
)]
trait FileChooser {
	fn open_file(&self, parent_window: &str, title: &str, options: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(interface = "org.freedesktop.portal.Request", default_service = "org.freedesktop.portal.Desktop")]
trait Request {
	#[zbus(signal)]
	fn response(&self, response: u32, results: HashMap<String, OwnedValue>) -> zbus::Result<()>;
}

/// Empty when the user cancels.
pub async fn pick_files(title: &str) -> Result<Vec<PathBuf>> {
	let conn = zbus::Connection::session().await?;
	// Subscribe before calling: the portal may answer before `open_file` returns its handle
	let sender = conn
		.unique_name()
		.expect("session bus assigns a unique name on connect")
		.trim_start_matches(':')
		.replace('.', "_");
	let token = format!("tg{}", std::process::id());
	let request = RequestProxy::builder(&conn)
		.path(format!("/org/freedesktop/portal/desktop/request/{sender}/{token}"))?
		.build()
		.await?;
	let mut responses = request.receive_response().await?;

	let options = HashMap::from([("handle_token", Value::from(token.as_str())), ("multiple", Value::from(true))]);
	FileChooserProxy::new(&conn).await?.open_file("", title, options).await?;

	let response = responses.next().await.ok_or_else(|| eyre!("portal dropped the request without answering"))?;
	let args = response.args()?;
	match args.response {
		0 => {}
		1 => return Ok(Vec::new()),
		n => bail!("file chooser failed (portal response {n})"),
	}
	let uris: Vec<String> = args.results.get("uris").ok_or_else(|| eyre!("portal success response without uris"))?.try_clone()?.try_into()?;
	uris.iter()
		.map(|uri| {
			let path = uri.strip_prefix("file://").ok_or_else(|| eyre!("portal returned a non-file uri: {uri}"))?;
			Ok(PathBuf::from(percent_encoding::percent_decode_str(path).decode_utf8()?.into_owned()))
		})
		.collect()
}
