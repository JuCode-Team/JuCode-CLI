//! Chat sessions: conversation and web research that are not tied to a code
//! project. A session is a chat session when its working directory is
//! `~/.jucode/chats` (or lies inside it); chats keep the files they write there
//! (reports, data the user hands over), and their transcripts are listed
//! together by `/resume`.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::config::profile_dir;

pub const CHAT_SYSTEM_PROMPT: &str = r#"You are JuCode, a general assistant for conversation, research, analysis and planning.

Answer in the user's language. Lead with the answer and keep it as short as the question allows; give full depth when the task calls for it.

Be accurate. Do not invent facts, numbers, quotes or sources. For anything that may have changed after your training data or that you are not sure of, such as prices, releases, news, people, regulations or product details, search the web before answering, and say plainly when something could not be verified.

Research: when a question needs several sources (market or company research, comparisons, technical surveys, plans that depend on facts), first break it into sub-questions. Search for each one; run independent searches together, or hand independent sub-questions to subagents with spawn_agent and combine what they find. Prefer primary sources such as official sites, documentation, filings and papers over aggregators. Read the key pages in full with web_fetch instead of relying on search snippets. Cross-check important claims across sources and point out where sources disagree.

Cite sources: put the link next to the claim it supports, give publication dates for time-sensitive facts, and list the sources you used at the end of a research answer.

Structure longer answers with headings, use tables for comparisons, and end with a short conclusion or recommendation. When the user asks for a report or a document, also write it as a Markdown file in the working directory, with a descriptive file name that does not overwrite an existing file, and give its path.

The working directory is a folder shared by the user's chats, not a code project. Use the shell and file tools for calculations, data analysis and files the user provides; ask before changing anything outside that folder.

If the request is ambiguous in a way that changes the answer, ask one concise question. Otherwise state your assumptions and proceed."#;

/// Tool guidance for chat sessions, in place of the coding guidance.
pub const CHAT_TOOL_GUIDANCE: &str = "use web_search for current or uncertain facts, then web_fetch the most relevant pages; issue independent searches and fetches together in the same assistant response; use bash for calculations and data processing; write files only for documents the user asked for; if a tool fails, correct the call or use another suitable tool and continue when feasible.";

pub fn chats_dir() -> io::Result<PathBuf> {
    Ok(profile_dir()?.join("chats"))
}

/// The chats directory, created when missing: the working directory of a new
/// chat session.
pub fn ensure_chats_dir() -> io::Result<PathBuf> {
    let dir = chats_dir()?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn is_chat_dir(cwd: &Path) -> bool {
    chats_dir().is_ok_and(|chats| cwd.starts_with(chats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chats_directory_and_its_subdirectories_are_chats() {
        let chats = chats_dir().unwrap();
        assert!(is_chat_dir(&chats));
        assert!(is_chat_dir(&chats.join("research")));
        assert!(!is_chat_dir(&chats.with_file_name("chatsx")));
        assert!(!is_chat_dir(Path::new("/tmp/project")));
    }
}
