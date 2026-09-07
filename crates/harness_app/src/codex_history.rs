use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{Context as _, bail};
#[cfg(test)]
use codex_app_server_client::ThreadItemEntry;
use codex_app_server_client::{CodexThread, CodexTurn};
use serde_json::Value;

const MAX_TIP_BYTES: u64 = 32 * 1024 * 1024;
const MAX_HEADER_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default, PartialEq)]
pub struct HistoryTip {
    turn_id: Option<String>,
    item_id: Option<String>,
}

impl HistoryTip {
    #[cfg(test)]
    pub fn verify(&self, entries: &[ThreadItemEntry]) -> anyhow::Result<()> {
        let turn_present = self
            .turn_id
            .as_ref()
            .is_none_or(|turn_id| entries.iter().any(|entry| &entry.turn_id == turn_id));
        let item_present = self
            .item_id
            .as_ref()
            .is_none_or(|item_id| entries.iter().any(|entry| &entry.item.id == item_id));
        if !turn_present || !item_present {
            bail!(
                "Codex's history index is behind the saved transcript. History is incomplete; live messages and your draft are kept. Repair the Codex history index, then refresh this conversation."
            );
        }
        Ok(())
    }

    pub fn verify_turns(&self, turns: &[CodexTurn]) -> anyhow::Result<()> {
        if self
            .turn_id
            .as_ref()
            .is_some_and(|id| !turns.iter().any(|turn| &turn.id == id))
            || self.item_id.as_ref().is_some_and(|id| {
                !turns
                    .iter()
                    .flat_map(|turn| &turn.items)
                    .any(|item| &item.id == id)
            })
        {
            bail!(
                "Codex's history index is behind the saved transcript. History is incomplete; live messages and your draft are kept. Repair the Codex history index, then refresh this conversation."
            );
        }
        Ok(())
    }
}

pub fn snapshot_error(
    tip: &anyhow::Result<Option<HistoryTip>>,
    thread: &CodexThread,
) -> Option<String> {
    match tip {
        Err(error) => Some(error.to_string()),
        Ok(Some(tip)) => tip
            .verify_turns(&thread.turns)
            .err()
            .map(|error| error.to_string()),
        Ok(None) => None,
    }
}

pub fn read_tip(path: Option<&Path>, thread_id: &str) -> anyhow::Result<Option<HistoryTip>> {
    let Some(path) = path else { return Ok(None) };
    let mut file = match File::open(path) {
        Ok(file) => file,
        // A remote App Server may report a path outside this machine.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Could not check saved Codex history"),
    };
    let length = file.metadata()?.len();
    let mut header = String::new();
    BufReader::new((&mut file).take(MAX_HEADER_BYTES)).read_line(&mut header)?;
    let metadata: Value = serde_json::from_str(&header).context("Invalid Codex history header")?;
    if metadata["type"] != "session_meta" || metadata["payload"]["id"] != thread_id {
        bail!("Saved Codex history belongs to a different conversation");
    }
    let start = length.saturating_sub(MAX_TIP_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(length - start).read_to_end(&mut bytes)?;
    parse_tip(&bytes, start != 0, thread_id)
}

fn parse_tip(
    bytes: &[u8],
    starts_mid_line: bool,
    thread_id: &str,
) -> anyhow::Result<Option<HistoryTip>> {
    let mut tip = HistoryTip::default();
    let mut rolled_back = false;
    // Sample before requesting pages, and ignore a trailing partial write.
    // Otherwise a newly streamed item could make a correct older page look stale.
    for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if (starts_mid_line && index == 0) || !line.ends_with(b"\n") {
            continue;
        }
        let row: Value =
            serde_json::from_slice(line).context("Invalid saved Codex history record")?;
        if row["type"] != "event_msg" {
            continue;
        }
        let payload = &row["payload"];
        match payload["type"].as_str() {
            Some("item_completed") => {
                if payload["thread_id"]
                    .as_str()
                    .is_some_and(|id| id != thread_id)
                {
                    bail!("Saved Codex event belongs to a different conversation");
                }
                tip.turn_id = payload["turn_id"].as_str().map(str::to_owned);
                tip.item_id = payload["item"]["id"].as_str().map(str::to_owned);
            }
            Some("task_started") => {
                tip.turn_id = payload["turn_id"].as_str().map(str::to_owned);
                tip.item_id = None;
            }
            Some("thread_rolled_back") => {
                // The preceding item may intentionally no longer be in history.
                // Do not mistake an explicit rollback for a stale index.
                tip = HistoryTip::default();
                rolled_back = true;
            }
            _ => {}
        }
    }
    if tip == HistoryTip::default() && starts_mid_line && !rolled_back {
        bail!("Could not verify the saved Codex history tip within the bounded read");
    }
    Ok((tip != HistoryTip::default()).then_some(tip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_new_turn_and_explicit_rollback_are_not_stale_items() {
        let tip = HistoryTip {
            turn_id: Some("new".into()),
            item_id: None,
        };
        assert!(
            tip.verify_turns(&[CodexTurn {
                id: "new".into(),
                status: json!("inProgress"),
                items: vec![]
            }])
            .is_ok()
        );
        let bytes = b"cut line\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_rolled_back\",\"num_turns\":1}}\n";
        assert_eq!(parse_tip(bytes, true, "thread").unwrap(), None);
    }

    fn entry(turn: &str, item: &str) -> ThreadItemEntry {
        serde_json::from_value(
            json!({"turnId":turn,"item":{"id":item,"type":"agentMessage","text":"answer"}}),
        )
        .unwrap()
    }

    #[test]
    fn stale_index_cannot_pass_with_an_old_overlap() {
        let tip = HistoryTip {
            turn_id: Some("new-turn".into()),
            item_id: Some("new-final".into()),
        };
        assert!(tip.verify(&[entry("old-turn", "old-cached")]).is_err());
        assert!(tip.verify(&[entry("new-turn", "new-final")]).is_ok());
    }

    #[test]
    fn tip_can_be_followed_by_newer_live_items() {
        let tip = HistoryTip {
            turn_id: Some("turn".into()),
            item_id: Some("sampled".into()),
        };
        assert!(
            tip.verify(&[entry("turn", "sampled"), entry("turn", "newer")])
                .is_ok()
        );
    }

    #[test]
    fn partial_edges_are_ignored_but_complete_bad_records_are_not() {
        let bytes = b"cut record\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"thread_id\":\"thread\",\"turn_id\":\"turn\",\"item\":{\"id\":\"final\"}}}\n{\"partial\":";
        let tip = parse_tip(bytes, true, "thread").unwrap().unwrap();
        assert!(tip.verify(&[entry("turn", "final")]).is_ok());
        assert!(parse_tip(b"bad record\n", false, "thread").is_err());
        assert!(parse_tip(bytes, true, "other-thread").is_err());
    }

    #[test]
    fn starting_a_turn_does_not_require_the_previous_turns_item() {
        let bytes = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"new-turn\"}}\n";
        let tip = parse_tip(bytes, false, "thread").unwrap().unwrap();
        assert!(tip.verify(&[entry("new-turn", "new-user")]).is_ok());
        assert!(tip.verify(&[entry("old-turn", "old-final")]).is_err());
    }
}
