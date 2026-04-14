pub mod content;
pub mod message;
pub mod session;

pub use content::*;
pub use message::*;
pub use session::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_header_round_trip() {
        let header = SessionHeader {
            entry_type: SessionHeaderType::Session,
            version: Some(3),
            id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            timestamp: "2024-12-03T14:00:00.000Z".to_string(),
            cwd: "/home/user/project".to_string(),
            parent_session: None,
        };
        let json = serde_json::to_string(&header).unwrap();
        let parsed: SessionHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(header, parsed);
        assert!(json.contains(r#""type":"session""#));
    }

    #[test]
    fn session_header_with_parent() {
        let header = SessionHeader {
            entry_type: SessionHeaderType::Session,
            version: Some(3),
            id: "id-1".to_string(),
            timestamp: "2024-12-03T14:00:00.000Z".to_string(),
            cwd: "/tmp".to_string(),
            parent_session: Some("/path/to/parent.jsonl".to_string()),
        };
        let json = serde_json::to_string(&header).unwrap();
        assert!(json.contains("parentSession"));
        let parsed: SessionHeader = serde_json::from_str(&json).unwrap();
        assert_eq!(header, parsed);
    }

    #[test]
    fn user_message_plain_text_round_trip() {
        let msg = AgentMessage::User(UserMessage {
            content: UserContent::Text("Hello world".to_string()),
            timestamp: 1701612000000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"user""#));
        assert!(json.contains(r#""content":"Hello world""#));
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn user_message_with_image_blocks() {
        let msg = AgentMessage::User(UserMessage {
            content: UserContent::Blocks(vec![
                UserContentBlock::Text(TextContent {
                    text: "Check this".to_string(),
                    text_signature: None,
                }),
                UserContentBlock::Image(ImageContent {
                    data: "base64data".to_string(),
                    mime_type: "image/png".to_string(),
                }),
            ]),
            timestamp: 1701612000000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn assistant_message_round_trip() {
        let msg = AgentMessage::Assistant(AssistantMessage {
            content: vec![
                AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: "Let me think...".to_string(),
                    thinking_signature: Some("sig123".to_string()),
                    redacted: None,
                }),
                AssistantContentBlock::Text(TextContent {
                    text: "Here is my answer.".to_string(),
                    text_signature: None,
                }),
                AssistantContentBlock::ToolCall(ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "/tmp/file.txt"}),
                    thought_signature: None,
                }),
            ],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            response_id: Some("resp_123".to_string()),
            usage: Usage {
                input: 100,
                output: 50,
                cache_read: 80,
                cache_write: 20,
                total_tokens: 250,
                cost: Some(UsageCost {
                    input: 0.001,
                    output: 0.002,
                    cache_read: 0.0005,
                    cache_write: 0.0003,
                    total: 0.0038,
                }),
            },
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 1701612001000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"assistant""#));
        assert!(json.contains(r#""stopReason":"toolUse""#));
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn tool_result_round_trip() {
        let msg = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "call_1".to_string(),
            tool_name: "read_file".to_string(),
            content: vec![ResultContentBlock::Text(TextContent {
                text: "file contents here".to_string(),
                text_signature: None,
            })],
            details: Some(serde_json::json!({"bytes": 1024})),
            is_error: false,
            timestamp: 1701612002000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"toolResult""#));
        assert!(json.contains(r#""isError":false"#));
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn bash_execution_round_trip() {
        let msg = AgentMessage::BashExecution(BashExecutionMessage {
            command: "ls -la".to_string(),
            output: "total 42\n".to_string(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            timestamp: 1701612003000,
            exclude_from_context: None,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"bashExecution""#));
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn message_entry_round_trip() {
        let entry = SessionEntry::Message(MessageEntry {
            base: SessionEntryBase {
                id: "a1b2c3d4".to_string(),
                parent_id: None,
                timestamp: "2024-12-03T14:00:01.000Z".to_string(),
            },
            message: AgentMessage::User(UserMessage {
                content: UserContent::Text("hi".to_string()),
                timestamp: 1701612001000,
            }),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"message""#));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn compaction_entry_round_trip() {
        let entry = SessionEntry::Compaction(CompactionEntry {
            base: SessionEntryBase {
                id: "deadbeef".to_string(),
                parent_id: Some("a1b2c3d4".to_string()),
                timestamp: "2024-12-03T15:00:00.000Z".to_string(),
            },
            summary: "## Summary\nDid some work.".to_string(),
            first_kept_entry_id: "a1b2c3d4".to_string(),
            tokens_before: 50000,
            details: Some(serde_json::json!({
                "readFiles": ["/tmp/a.rs"],
                "modifiedFiles": ["/tmp/b.rs"]
            })),
            from_hook: None,
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"compaction""#));
        assert!(json.contains("firstKeptEntryId"));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn model_change_entry_round_trip() {
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            base: SessionEntryBase {
                id: "11223344".to_string(),
                parent_id: Some("deadbeef".to_string()),
                timestamp: "2024-12-03T15:01:00.000Z".to_string(),
            },
            provider: "anthropic".to_string(),
            model_id: "claude-sonnet-4-20250514".to_string(),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"model_change""#));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn thinking_level_change_round_trip() {
        let entry = SessionEntry::ThinkingLevelChange(ThinkingLevelChangeEntry {
            base: SessionEntryBase {
                id: "aabbccdd".to_string(),
                parent_id: Some("11223344".to_string()),
                timestamp: "2024-12-03T15:02:00.000Z".to_string(),
            },
            thinking_level: "high".to_string(),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"thinking_level_change""#));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn custom_entry_round_trip() {
        let entry = SessionEntry::Custom(CustomEntry {
            base: SessionEntryBase {
                id: "ee001122".to_string(),
                parent_id: Some("aabbccdd".to_string()),
                timestamp: "2024-12-03T15:03:00.000Z".to_string(),
            },
            custom_type: "my-extension".to_string(),
            data: Some(serde_json::json!({"key": "value"})),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"custom""#));
        assert!(json.contains("customType"));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn custom_message_entry_round_trip() {
        let entry = SessionEntry::CustomMessage(CustomMessageEntry {
            base: SessionEntryBase {
                id: "ff334455".to_string(),
                parent_id: Some("ee001122".to_string()),
                timestamp: "2024-12-03T15:04:00.000Z".to_string(),
            },
            custom_type: "ext".to_string(),
            content: CustomContent::Text("injected text".to_string()),
            details: None,
            display: true,
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"custom_message""#));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn label_entry_round_trip() {
        let entry = SessionEntry::Label(LabelEntry {
            base: SessionEntryBase {
                id: "99887766".to_string(),
                parent_id: Some("ff334455".to_string()),
                timestamp: "2024-12-03T15:05:00.000Z".to_string(),
            },
            target_id: "a1b2c3d4".to_string(),
            label: Some("important".to_string()),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"label""#));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn session_info_entry_round_trip() {
        let entry = SessionEntry::SessionInfo(SessionInfoEntry {
            base: SessionEntryBase {
                id: "00112233".to_string(),
                parent_id: Some("99887766".to_string()),
                timestamp: "2024-12-03T15:06:00.000Z".to_string(),
            },
            name: Some("my session".to_string()),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"session_info""#));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn branch_summary_entry_round_trip() {
        let entry = SessionEntry::BranchSummary(BranchSummaryEntry {
            base: SessionEntryBase {
                id: "bbaa0011".to_string(),
                parent_id: Some("00112233".to_string()),
                timestamp: "2024-12-03T15:07:00.000Z".to_string(),
            },
            from_id: "a1b2c3d4".to_string(),
            summary: "Explored alternative approach".to_string(),
            details: None,
            from_hook: Some(false),
        });
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"branch_summary""#));
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn file_entry_header_round_trip() {
        let header = FileEntry::Header(SessionHeader {
            entry_type: SessionHeaderType::Session,
            version: Some(3),
            id: "uuid-1".to_string(),
            timestamp: "2024-12-03T14:00:00.000Z".to_string(),
            cwd: "/tmp".to_string(),
            parent_session: None,
        });
        let json = serde_json::to_string(&header).unwrap();
        let parsed: FileEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(header, parsed);
    }

    #[test]
    fn file_entry_session_entry_round_trip() {
        let entry = FileEntry::Entry(Box::new(SessionEntry::Message(MessageEntry {
            base: SessionEntryBase {
                id: "a1b2c3d4".to_string(),
                parent_id: None,
                timestamp: "2024-12-03T14:00:01.000Z".to_string(),
            },
            message: AgentMessage::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 1701612001000,
            }),
        })));
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: FileEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn jsonl_multi_line_round_trip() {
        let header = SessionHeader {
            entry_type: SessionHeaderType::Session,
            version: Some(3),
            id: "session-1".to_string(),
            timestamp: "2024-12-03T14:00:00.000Z".to_string(),
            cwd: "/home/user".to_string(),
            parent_session: None,
        };
        let entry = SessionEntry::Message(MessageEntry {
            base: SessionEntryBase {
                id: "e1e2e3e4".to_string(),
                parent_id: None,
                timestamp: "2024-12-03T14:00:01.000Z".to_string(),
            },
            message: AgentMessage::User(UserMessage {
                content: UserContent::Text("test".to_string()),
                timestamp: 1701612001000,
            }),
        });

        // Serialize as JSONL
        let line1 = serde_json::to_string(&header).unwrap();
        let line2 = serde_json::to_string(&entry).unwrap();
        let jsonl = format!("{}\n{}\n", line1, line2);

        // Parse back line by line
        let mut lines = jsonl.lines();
        let parsed_header: SessionHeader = serde_json::from_str(lines.next().unwrap()).unwrap();
        let parsed_entry: SessionEntry = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(header, parsed_header);
        assert_eq!(entry, parsed_entry);
    }

    #[test]
    fn stop_reason_serializes_camel_case() {
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            r#""toolUse""#
        );
        assert_eq!(
            serde_json::to_string(&StopReason::Stop).unwrap(),
            r#""stop""#
        );
    }

    #[test]
    fn custom_message_role_round_trip() {
        let msg = AgentMessage::Custom(CustomMessage {
            custom_type: "ext-abc".to_string(),
            content: CustomContent::Blocks(vec![ResultContentBlock::Text(TextContent {
                text: "injected".to_string(),
                text_signature: None,
            })]),
            display: false,
            details: None,
            timestamp: 1701612004000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"custom""#));
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn branch_summary_message_round_trip() {
        let msg = AgentMessage::BranchSummary(BranchSummaryMessage {
            summary: "explored option A".to_string(),
            from_id: "abc12345".to_string(),
            timestamp: 1701612005000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"branchSummary""#));
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn compaction_summary_message_round_trip() {
        let msg = AgentMessage::CompactionSummary(CompactionSummaryMessage {
            summary: "compacted 50k tokens".to_string(),
            tokens_before: 50000,
            timestamp: 1701612006000,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"compactionSummary""#));
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, parsed);
    }
}
