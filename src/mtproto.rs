use std::{io::Write as _, path::PathBuf, sync::Arc};

use eyre::{Result, bail, eyre};
use grammers_client::{Client, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use grammers_tl_types as tl;
use tracing::{debug, error, info, warn};

use crate::config::{AppConfig, TopicsMetadata, telegram_chat_id};

/// Get the session file path (same convention as social_networks)
fn session_path(username: &str) -> PathBuf {
	v_utils::xdg_state_file!(&format!("{}.session", username))
}

/// Create and authenticate a Telegram MTProto client.
/// Uses credentials from the app config, with env var fallbacks.
pub async fn create_client(config: &AppConfig) -> Result<(Client, tokio::task::JoinHandle<()>)> {
	let api_id = config.api_id.ok_or_else(|| eyre!("api_id not configured"))?;
	let api_hash = config
		.api_hash
		.clone()
		.or_else(|| std::env::var("TELEGRAM_API_HASH").ok())
		.ok_or_else(|| eyre!("api_hash not configured (set in config or TELEGRAM_API_HASH env)"))?;
	let phone = config
		.phone
		.clone()
		.or_else(|| std::env::var("PHONE_NUMBER_FR").ok())
		.ok_or_else(|| eyre!("phone not configured (set in config or PHONE_NUMBER_FR env)"))?;
	let username = config.username.clone().unwrap_or_else(|| "@user".to_string());

	let session_file = session_path(&username);
	info!("Using session file: {}", session_file.display());

	// Ensure parent directory exists
	if let Some(parent) = session_file.parent() {
		std::fs::create_dir_all(parent)?;
	}

	let session = match SqliteSession::open(&session_file) {
		Ok(s) => Arc::new(s),
		Err(e) => {
			let err_str = e.to_string();
			if err_str.contains("not a database") || err_str.contains("code 26") {
				error!("Session database is corrupted: {e}");
				info!("Deleting corrupted session file and creating a new one");
				std::fs::remove_file(&session_file)?;
				Arc::new(SqliteSession::open(&session_file)?)
			} else {
				return Err(e.into());
			}
		}
	};

	info!("Connecting to Telegram with api_id: {}", api_id);
	let pool = SenderPool::new(Arc::clone(&session), api_id);
	let client = Client::new(&pool);

	let SenderPool { runner, .. } = pool;
	let handle = tokio::spawn(runner.run());

	if !client.is_authorized().await? {
		info!("Not authorized, requesting login code for {}", phone);
		let token = client.request_login_code(&phone, &api_hash).await?;
		info!("Login code requested successfully, check your Telegram app");

		print!("Enter the code you received: ");
		std::io::stdout().flush()?;
		let mut code = String::new();
		std::io::stdin().read_line(&mut code)?;
		let code = code.trim();

		match client.sign_in(&token, code).await {
			Ok(_) => {
				eprintln!("Sign in successful!");
				info!("Sign in successful");
			}
			Err(SignInError::PasswordRequired(password_token)) => {
				info!("2FA password required");
				print!("Enter your 2FA password: ");
				std::io::stdout().flush()?;
				let mut password = String::new();
				std::io::stdin().read_line(&mut password)?;
				let password = password.trim();

				client.check_password(password_token, password).await?;
				eprintln!("2FA authentication successful!");
				info!("2FA authentication successful");
			}
			Err(e) => {
				error!("Sign in failed: {e}");
				bail!("Failed to sign in: {}", e);
			}
		}

		info!("Session saved to {}", session_file.display());
	}

	info!("Telegram client authorized");
	Ok((client, handle))
}

/// Discovered forum topic
#[derive(Clone, Debug)]
pub struct DiscoveredTopic {
	pub topic_id: i32,
	pub title: String,
}

/// Fetch all forum topics for a group using MTProto
pub async fn fetch_forum_topics(client: &Client, group_id: u64) -> Result<Vec<DiscoveredTopic>> {
	let chat_id = telegram_chat_id(group_id);

	// Get the InputPeer by iterating dialogs
	let input_peer = get_input_peer(client, chat_id).await?;

	let mut topics = Vec::new();
	let mut offset_date = 0;
	let mut offset_id = 0;
	let mut offset_topic = 0;

	loop {
		let request = tl::functions::messages::GetForumTopics {
			peer: input_peer.clone(),
			q: None,
			offset_date,
			offset_id,
			offset_topic,
			limit: 100,
		};

		let result = client.invoke(&request).await?;

		let forum_topics = match result {
			tl::enums::messages::ForumTopics::Topics(t) => t,
		};

		if forum_topics.topics.is_empty() {
			break;
		}

		for topic in &forum_topics.topics {
			match topic {
				tl::enums::ForumTopic::Topic(t) => {
					topics.push(DiscoveredTopic {
						topic_id: t.id,
						title: t.title.clone(),
					});
					debug!("Found topic: {} (id={})", t.title, t.id);
				}
				tl::enums::ForumTopic::Deleted(d) => {
					debug!("Skipping deleted topic: id={}", d.id);
				}
			}
		}

		// Check if we got all topics
		if topics.len() >= forum_topics.count as usize {
			break;
		}

		// Update offsets for pagination
		if let Some(last) = forum_topics.topics.last() {
			match last {
				tl::enums::ForumTopic::Topic(t) => {
					offset_date = t.date;
					offset_id = t.top_message;
					offset_topic = t.id;
				}
				tl::enums::ForumTopic::Deleted(d) => {
					offset_topic = d.id;
				}
			}
		} else {
			break;
		}
	}

	info!("Discovered {} topics in group {}", topics.len(), group_id);
	Ok(topics)
}

/// Get InputPeer from chat_id by iterating dialogs
async fn get_input_peer(client: &Client, chat_id: i64) -> Result<tl::enums::InputPeer> {
	let mut dialogs = client.iter_dialogs();

	// chat_id is like -1002244305221, we want 2244305221
	let expected_id = if chat_id < 0 {
		let s = chat_id.to_string();
		if s.starts_with("-100") { s[4..].parse::<i64>().unwrap_or(0) } else { chat_id.abs() }
	} else {
		chat_id
	};

	while let Some(dialog) = dialogs.next().await? {
		// Check if this is the right peer by examining the raw dialog data
		match &dialog.raw {
			tl::enums::Dialog::Dialog(d) => {
				let peer_id = match &d.peer {
					tl::enums::Peer::Channel(c) => c.channel_id,
					tl::enums::Peer::Chat(c) => c.chat_id,
					tl::enums::Peer::User(u) => u.user_id,
				};

				if peer_id == expected_id {
					// Use the peer from dialog to get InputPeer
					let peer = dialog.peer();
					match peer {
						grammers_client::types::Peer::Group(g) => {
							// Get access_hash from raw channel data
							if let tl::enums::Chat::Channel(ch) = &g.raw {
								return Ok(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
									channel_id: ch.id,
									access_hash: ch.access_hash.unwrap_or(0),
								}));
							}
						}
						grammers_client::types::Peer::Channel(c) => {
							return Ok(tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
								channel_id: c.raw.id,
								access_hash: c.raw.access_hash.unwrap_or(0),
							}));
						}
						_ => {}
					}
				}
			}
			tl::enums::Dialog::Folder(_) => continue,
		}
	}

	bail!("Could not find channel with id {} in dialogs. Make sure the user account has access to this group.", chat_id)
}

/// Discover and update topics metadata for all configured groups
pub async fn discover_all_topics(config: &AppConfig) -> Result<()> {
	// Check if credentials are configured (either in config or env vars)
	let has_api_id = config.api_id.is_some();
	let has_api_hash = config.api_hash.is_some() || std::env::var("TELEGRAM_API_HASH").is_ok();
	let has_phone = config.phone.is_some() || std::env::var("PHONE_NUMBER_FR").is_ok();

	if !has_api_id || !has_api_hash || !has_phone {
		warn!("Telegram MTProto credentials not configured (api_id/api_hash/phone).");
		warn!("Topic discovery will be skipped.");
		return Ok(());
	}

	let (client, handle) = create_client(config).await?;
	let mut metadata = TopicsMetadata::load();

	for group_id in config.forum_group_ids() {
		info!("Fetching topics for group {}...", group_id);

		match fetch_forum_topics(&client, group_id).await {
			Ok(topics) =>
				for topic in topics {
					let sanitized_name = sanitize_topic_name(&topic.title);
					metadata.set_topic_name(group_id, topic.topic_id as u64, sanitized_name);
				},
			Err(e) => {
				warn!("Failed to fetch topics for group {}: {}", group_id, e);
			}
		}
	}

	metadata.save()?;
	info!("Topics metadata updated");

	// Disconnect client
	client.disconnect();
	handle.abort();

	Ok(())
}

/// Sanitize topic name for use as filename
fn sanitize_topic_name(name: &str) -> String {
	name.to_lowercase()
		.chars()
		.map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
		.collect::<String>()
		.trim_matches('_')
		.to_string()
}

/// Delete messages from a channel/supergroup via MTProto
/// Returns the number of successfully deleted messages (including already-deleted ones)
pub async fn delete_messages(client: &Client, group_id: u64, message_ids: &[i32]) -> Result<usize> {
	if message_ids.is_empty() {
		return Ok(0);
	}

	let chat_id = telegram_chat_id(group_id);
	let input_peer = get_input_peer(client, chat_id).await?;

	// Extract channel info from InputPeer
	let (channel_id, access_hash) = match &input_peer {
		tl::enums::InputPeer::Channel(c) => (c.channel_id, c.access_hash),
		_ => bail!("Expected channel peer for group {}", group_id),
	};

	debug!(group_id, channel_id, ?message_ids, "Attempting to delete messages");

	// First, verify the messages exist by fetching them
	let get_request = tl::functions::channels::GetMessages {
		channel: tl::enums::InputChannel::Channel(tl::types::InputChannel { channel_id, access_hash }),
		id: message_ids.iter().map(|&id| tl::enums::InputMessage::Id(tl::types::InputMessageId { id })).collect(),
	};

	// Track how many messages are already deleted (Empty) - we'll count these as "successful"
	let mut already_deleted_count = 0usize;

	match client.invoke(&get_request).await {
		Ok(result) => {
			let messages = match &result {
				tl::enums::messages::Messages::Messages(m) => &m.messages,
				tl::enums::messages::Messages::Slice(m) => &m.messages,
				tl::enums::messages::Messages::ChannelMessages(m) => &m.messages,
				tl::enums::messages::Messages::NotModified(_) => &vec![],
			};

			for msg in messages {
				match msg {
					tl::enums::Message::Message(m) => {
						debug!(msg_id = m.id, "Message exists, will delete");
					}
					tl::enums::Message::Service(s) => {
						debug!(msg_id = s.id, "Service message, will delete");
					}
					tl::enums::Message::Empty(e) => {
						// Message already deleted on Telegram - count as success
						info!(msg_id = e.id, "Message already deleted on Telegram, treating as successful deletion");
						already_deleted_count += 1;
					}
				}
			}
		}
		Err(e) => {
			warn!(error = %e, "Failed to verify messages exist before deletion");
		}
	}

	// If all messages are already deleted, skip the delete call
	if already_deleted_count == message_ids.len() {
		info!(count = already_deleted_count, "All messages already deleted on Telegram");
		return Ok(already_deleted_count);
	}

	let channel = tl::enums::InputChannel::Channel(tl::types::InputChannel { channel_id, access_hash });
	let request = tl::functions::channels::DeleteMessages { channel, id: message_ids.to_vec() };

	let result = client.invoke(&request).await?;

	let pts_count = match result {
		tl::enums::messages::AffectedMessages::Messages(m) => m.pts_count,
	};

	// Total successful = actually deleted + already deleted
	let total_success = (pts_count as usize) + already_deleted_count;
	info!(pts_count, already_deleted_count, total_success, "Delete operation complete");
	Ok(total_success)
}

/// Edit a message in a channel/supergroup via MTProto
pub async fn edit_message(client: &Client, group_id: u64, message_id: i32, new_text: &str) -> Result<()> {
	let chat_id = telegram_chat_id(group_id);
	let input_peer = get_input_peer(client, chat_id).await?;

	let request = tl::functions::messages::EditMessage {
		peer: input_peer,
		id: message_id,
		message: Some(new_text.to_string()),
		no_webpage: false,
		invert_media: false,
		media: None,
		reply_markup: None,
		entities: None,
		schedule_date: None,
		quick_reply_shortcut_id: None,
	};

	client.invoke(&request).await?;
	info!("Edited message {} in group {}", message_id, group_id);
	Ok(())
}
