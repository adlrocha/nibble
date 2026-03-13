//! Native messaging host for browser extension.
//! Receives task updates from extension and writes to nibble database.

use nibble::db::{default_db_path, Database};
use nibble::models::{Task, TaskContext, TaskStatus};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    #[serde(rename = "type")]
    msg_type: String,
    task_id: String,
    agent_type: String,
    status: String,
    title: String,
    context: MessageContext,
}

#[derive(Debug, Deserialize)]
struct MessageContext {
    url: Option<String>,
    conversation_id: Option<String>,
    #[allow(dead_code)]
    timestamp: Option<i64>,
    duration_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
struct OutgoingMessage {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

// Read a message from stdin using Chrome native messaging protocol
// Format: 4-byte length (little-endian) + JSON message
fn read_message() -> Result<IncomingMessage> {
    let mut length_bytes = [0u8; 4];
    io::stdin()
        .read_exact(&mut length_bytes)
        .context("Failed to read message length")?;

    let length = u32::from_le_bytes(length_bytes) as usize;

    // Sanity check: messages shouldn't be > 1MB
    if length > 1_048_576 {
        anyhow::bail!("Message too large: {} bytes", length);
    }

    let mut buffer = vec![0u8; length];
    io::stdin()
        .read_exact(&mut buffer)
        .context("Failed to read message body")?;

    let message: IncomingMessage =
        serde_json::from_slice(&buffer).context("Failed to parse JSON message")?;

    Ok(message)
}

// Write a message to stdout using Chrome native messaging protocol
fn write_message(message: &OutgoingMessage) -> Result<()> {
    let json = serde_json::to_string(message)?;
    let length = json.len() as u32;

    io::stdout()
        .write_all(&length.to_le_bytes())
        .context("Failed to write message length")?;

    io::stdout()
        .write_all(json.as_bytes())
        .context("Failed to write message body")?;

    io::stdout().flush()?;

    Ok(())
}

fn process_message(db: &Database, message: IncomingMessage) -> Result<()> {
    eprintln!(
        "Processing message: {} {} {}",
        message.msg_type, message.status, message.task_id
    );

    match message.status.as_str() {
        "running" => {
            // Check if task already exists (for follow-up messages)
            if let Some(mut existing_task) = db.get_task_by_id(&message.task_id)? {
                // Task exists - update to running (for follow-ups)
                existing_task.status = TaskStatus::Running;
                existing_task.updated_at = chrono::Utc::now();
                existing_task.completed_at = None; // Clear completion timestamp

                db.update_task(&existing_task)?;
                eprintln!("Updated existing task to running: {}", message.task_id);
            } else {
                // Task doesn't exist - create new one
                let mut task = Task::new(
                    message.task_id.clone(),
                    message.agent_type,
                    message.title,
                    None, // No PID for web tasks
                    None,
                );

                // Add context
                let mut extra = HashMap::new();
                if let Some(conv_id) = message.context.conversation_id {
                    extra.insert("conversation_id".to_string(), serde_json::json!(conv_id));
                }
                if let Some(duration) = message.context.duration_ms {
                    extra.insert("duration_ms".to_string(), serde_json::json!(duration));
                }

                task.context = Some(TaskContext {
                    url: message.context.url,
                    project_path: None,
                    session_id: None,
                    extra,
                });

                db.insert_task(&task)?;
                eprintln!("Created new task: {}", message.task_id);
            }
        }
        "completed" => {
            // Update existing task to completed (finished generating, waiting for user)
            if let Some(mut task) = db.get_task_by_id(&message.task_id)? {
                task.complete();
                db.update_task(&task)?;

                eprintln!("Completed task: {}", message.task_id);
            } else {
                eprintln!("Task not found: {}", message.task_id);
            }
        }
        "exited" => {
            // Update existing task to exited (tab closed / process terminated)
            if let Some(mut task) = db.get_task_by_id(&message.task_id)? {
                task.set_exited(None);
                db.update_task(&task)?;

                eprintln!("Task exited: {}", message.task_id);
            } else {
                eprintln!("Task not found: {}", message.task_id);
            }
        }
        _ => {
            eprintln!("Unknown status: {}", message.status);
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    // Note: stderr output goes to browser console/logs
    // For debugging, check: chrome://extensions -> Agent Inbox -> background page -> console

    eprintln!("agent-bridge started");

    // Open database
    let db_path = default_db_path();
    let db = Database::open(&db_path).context("Failed to open database")?;

    eprintln!("Database opened: {:?}", db_path);

    // Main message loop
    loop {
        match read_message() {
            Ok(message) => {
                eprintln!("Received message: {:?}", message);

                match process_message(&db, message) {
                    Ok(()) => {
                        let response = OutgoingMessage {
                            status: "ok".to_string(),
                            message: None,
                        };
                        if let Err(e) = write_message(&response) {
                            eprintln!("Failed to write response: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error processing message: {}", e);
                        let response = OutgoingMessage {
                            status: "error".to_string(),
                            message: Some(e.to_string()),
                        };
                        if let Err(e) = write_message(&response) {
                            eprintln!("Failed to write error response: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("Error reading message: {}", e);
                // EOF or error, exit gracefully
                break;
            }
        }
    }

    eprintln!("agent-bridge exiting");

    Ok(())
}
