//! Disposable, exact-generation FTS5 projection for supervisor memory.
//!
//! The caller owns the source documents and their generation. Rebuilding
//! replaces the complete projection in one SQLite transaction. Search rejects
//! a stale requested generation so callers can catch the index up first.
//!
//! The bundled SQLite trigram tokenizer matches literal substrings of three
//! or more Unicode characters. Queries shorter than three characters use
//! SQLite `instr` against the same scoped rows; this keeps short Japanese
//! terms and short code fragments searchable without FTS query syntax.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

const MAX_QUERY_CHARS: usize = 256;
const MAX_RESULTS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: String,
    pub revision: u64,
    pub workspace: String,
    pub team: String,
    pub routing: String,
    pub title: String,
    pub body: String,
}

/// Exact authorization scope applied before returning search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub workspace: String,
    pub team: String,
    pub routing: String,
}

/// A bounded search hit. The snippet is generated from the indexed body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResults {
    pub indexed_generation: u64,
    pub results: Vec<SearchResult>,
}

/// Returns the indexed generation, or `None` when the derived database is
/// absent. A damaged or incompatible database returns an error so callers can
/// rebuild it from the committed Markdown source.
pub fn generation(path: &Path) -> Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("could not open supervisor index {}", path.display()))?;
    let generation: i64 = connection
        .query_row(
            "SELECT generation FROM index_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .context("supervisor index generation is missing")?;
    Ok(Some(
        u64::try_from(generation).context("supervisor index generation is negative")?,
    ))
}

/// Atomically replaces all documents with the exact committed `generation`.
/// Older generations are rejected. Rebuilding the same generation is allowed
/// for recovery from a deleted or damaged disposable database.
pub fn rebuild(path: &Path, generation: u64, documents: &[Document]) -> Result<u64> {
    let generation = i64::try_from(generation).context("index generation exceeds SQLite range")?;
    let mut ids = HashSet::with_capacity(documents.len());
    for document in documents {
        ensure!(!document.id.is_empty(), "document id must not be empty");
        ensure!(
            ids.insert(&document.id),
            "duplicate document id `{}`",
            document.id
        );
        i64::try_from(document.revision).context("document revision exceeds SQLite range")?;
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create index directory {}", parent.display()))?;
    }
    let mut connection = Connection::open(path)
        .with_context(|| format!("could not open supervisor index {}", path.display()))?;
    schema(&connection)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = transaction.query_row(
        "SELECT generation FROM index_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        generation >= current,
        "cannot replace index generation {current} with older generation {generation}"
    );
    transaction.execute("DELETE FROM documents_fts", [])?;
    {
        let mut insert = transaction.prepare(
            "INSERT INTO documents_fts (id, revision, workspace, team, routing, title, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for document in documents {
            insert.execute(params![
                document.id,
                i64::try_from(document.revision)?,
                document.workspace,
                document.team,
                document.routing,
                document.title,
                document.body,
            ])?;
        }
    }
    transaction.execute(
        "UPDATE index_state SET generation = ?1 WHERE singleton = 1",
        [generation],
    )?;
    transaction.commit()?;
    Ok(generation as u64)
}

/// Searches plain text in one exact workspace/team/routing scope.
///
/// Queries are limited to 256 Unicode characters and results to 50. FTS
/// metacharacters are quoted as literal text. One- and two-character queries
/// use `instr`, since the trigram tokenizer has no token for them.
pub fn search(
    path: &Path,
    expected_generation: u64,
    scope: &Scope,
    query: &str,
    limit: usize,
) -> Result<SearchResults> {
    let query = query.trim();
    let query_chars = query.chars().count();
    ensure!(query_chars > 0, "search query must not be empty");
    ensure!(
        query_chars <= MAX_QUERY_CHARS,
        "search query exceeds {MAX_QUERY_CHARS} characters"
    );
    let expected = i64::try_from(expected_generation)
        .context("expected index generation exceeds SQLite range")?;
    ensure!(
        path.is_file(),
        "supervisor search index is missing and needs rebuild"
    );
    let connection = Connection::open(path)
        .with_context(|| format!("could not open supervisor index {}", path.display()))?;
    schema(&connection)?;
    let indexed: i64 = connection.query_row(
        "SELECT generation FROM index_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if indexed != expected {
        bail!("supervisor index generation mismatch: expected {expected}, indexed {indexed}");
    }
    let limit = limit.clamp(1, MAX_RESULTS) as i64;
    let mut results = Vec::new();
    if query_chars < 3 {
        let mut statement = connection.prepare(
            "SELECT id, revision, title,
                    CASE WHEN instr(body, ?1) > 0
                         THEN substr(body, max(instr(body, ?1) - 80, 1), 180)
                         ELSE substr(title, max(instr(title, ?1) - 80, 1), 180) END
             FROM documents_fts
             WHERE workspace = ?2 AND team = ?3 AND routing = ?4
               AND (instr(body, ?1) > 0 OR instr(title, ?1) > 0)
             LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![query, scope.workspace, scope.team, scope.routing, limit],
            result_row,
        )?;
        for row in rows {
            results.push(row?);
        }
    } else {
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let mut statement = connection.prepare(
            "SELECT id, revision, title, snippet(documents_fts, 6, '<mark>', '</mark>', '…', 24)
             FROM documents_fts
             WHERE documents_fts MATCH ?1
               AND workspace = ?2 AND team = ?3 AND routing = ?4
             ORDER BY rank
             LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![phrase, scope.workspace, scope.team, scope.routing, limit],
            result_row,
        )?;
        for row in rows {
            results.push(row?);
        }
    }
    Ok(SearchResults {
        indexed_generation: indexed as u64,
        results,
    })
}

fn schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS index_state (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             generation INTEGER NOT NULL
         );
         INSERT OR IGNORE INTO index_state (singleton, generation) VALUES (1, 0);
         CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
             id UNINDEXED, revision UNINDEXED, workspace UNINDEXED, team UNINDEXED,
             routing UNINDEXED, title, body,
             tokenize = 'trigram'
         );",
    )?;
    Ok(())
}

fn result_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchResult> {
    Ok(SearchResult {
        id: row.get(0)?,
        revision: u64::try_from(row.get::<_, i64>(1)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        title: row.get(2)?,
        snippet: row.get(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(id: &str, workspace: &str, team: &str, routing: &str, body: &str) -> Document {
        Document {
            id: id.into(),
            revision: 1,
            workspace: workspace.into(),
            team: team.into(),
            routing: routing.into(),
            title: format!("{id} title"),
            body: body.into(),
        }
    }

    fn scope(workspace: &str, team: &str, routing: &str) -> Scope {
        Scope {
            workspace: workspace.into(),
            team: team.into(),
            routing: routing.into(),
        }
    }

    #[test]
    fn rebuild_replaces_exact_generation_and_rejects_stale_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let one = document("one", "acme", "DATA", "default", "retry the flaky command");
        assert_eq!(rebuild(&path, 3, &[one]).unwrap(), 3);
        let found = search(&path, 3, &scope("acme", "DATA", "default"), "flaky", 10).unwrap();
        assert_eq!(found.indexed_generation, 3);
        assert_eq!(found.results.len(), 1);

        let two = document("two", "acme", "DATA", "default", "avoid the slow retry");
        assert_eq!(rebuild(&path, 4, &[two]).unwrap(), 4);
        assert!(
            search(&path, 3, &scope("acme", "DATA", "default"), "flaky", 10)
                .unwrap_err()
                .to_string()
                .contains("generation mismatch")
        );
        assert!(
            search(&path, 4, &scope("acme", "DATA", "default"), "flaky", 10)
                .unwrap()
                .results
                .is_empty()
        );
    }

    #[test]
    fn search_does_not_create_a_missing_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sqlite3");
        assert!(search(&path, 1, &scope("acme", "DATA", "default"), "lesson", 10).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn search_isolated_to_exact_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let docs = [
            document("a", "acme", "DATA", "default", "shared deploy failure"),
            document("b", "other", "DATA", "default", "shared deploy failure"),
            document("c", "acme", "WEB", "default", "shared deploy failure"),
            document("d", "acme", "DATA", "special", "shared deploy failure"),
        ];
        rebuild(&path, 1, &docs).unwrap();
        let found = search(
            &path,
            1,
            &scope("acme", "DATA", "default"),
            "deploy failure",
            10,
        )
        .unwrap();
        assert_eq!(
            found
                .results
                .iter()
                .map(|hit| hit.id.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn fts_query_metacharacters_are_literal_and_unicode_substrings_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        let docs = [
            document("jp", "acme", "DATA", "default", "再試行すると成功した"),
            document(
                "code",
                "acme",
                "DATA",
                "default",
                "Call foo::bar() after retry",
            ),
        ];
        rebuild(&path, 1, &docs).unwrap();
        let scope = scope("acme", "DATA", "default");
        let japanese = search(&path, 1, &scope, "試行する", 10).unwrap();
        assert_eq!(japanese.results[0].id, "jp");
        let code = search(&path, 1, &scope, "foo::bar()", 10).unwrap();
        assert_eq!(code.results[0].id, "code");
        let literal = search(&path, 1, &scope, "foo*bar", 10).unwrap();
        assert!(literal.results.is_empty());
    }

    #[test]
    fn one_and_two_character_queries_use_literal_substrings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.sqlite3");
        rebuild(
            &path,
            1,
            &[document(
                "jp",
                "acme",
                "DATA",
                "default",
                "障害の原因を調査",
            )],
        )
        .unwrap();
        let scope = scope("acme", "DATA", "default");
        assert_eq!(
            search(&path, 1, &scope, "障", 10).unwrap().results[0].id,
            "jp"
        );
        assert_eq!(
            search(&path, 1, &scope, "原因", 10).unwrap().results[0].id,
            "jp"
        );
    }
}
