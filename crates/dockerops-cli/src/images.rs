//! What is on disk, largest first — and `docker system df` without the rest of it.
//!
//! `docker images` sorts by creation date, which is almost never the question: the
//! question is what is taking the space, or whether a tag is the digest it is supposed to
//! be. Both tools here are small, and both exist so that a disk-space question costs one
//! call rather than `docker images | sort -k7 -h | head`.

use serde::Serialize;

use crate::{
    Result,
    invoke::{self, Invocation, safe_value},
    runner::CommandRunner,
    text::{elide, human, pad, parse_size, plural, width_of},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Biggest first — the reason this tool exists.
    #[default]
    Size,
    /// Newest first, as `docker images` does it.
    Created,
    Name,
}

#[derive(Debug, Clone, Default)]
pub struct ImagesRequest {
    pub invocation: Invocation,
    /// Only images matching these references: `ghcr.io/acme/*`, `postgres`.
    pub patterns: Vec<String>,
    /// Docker filters, as `key=value`: `dangling=true`, `label=…`, `before=…`.
    pub filter: Vec<String>,
    /// Include intermediate layers (`--all`). Off by default, as in the CLI.
    pub all: bool,
    /// Include untagged images. On by default: they are usually the disk space.
    pub dangling: Option<bool>,
    pub sort: Sort,
    /// Cap on rows. Defaults to 50.
    pub limit: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct Image {
    /// `repository:tag`, or `<none>:<none>` for a dangling image.
    pub reference: String,
    pub id: String,
    /// Bytes, parsed from Docker's own human figure so the sort is numeric.
    pub size: u64,
    pub created: String,
}

#[derive(Debug, Serialize)]
pub struct ImagesOutcome {
    /// The rendered text. This is what a caller should show; the rest is for machines.
    ///
    /// Serialized deliberately. Hosts differ over which half of a response they display,
    /// and one that reads only `structuredContent` renders nothing without this field.
    pub text: String,
    pub images: Vec<Image>,
    pub command: String,
    /// Bytes across the images listed. Not the same as disk usage: layers are shared, and
    /// `usage` is the tool that knows the difference.
    pub total_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

const DEFAULT_LIMIT: usize = 50;

pub fn images(runner: &dyn CommandRunner, request: &ImagesRequest) -> Result<ImagesOutcome> {
    for pattern in &request.patterns {
        safe_value(pattern, "pattern")?;
    }
    let mut rest = vec![
        "images".to_owned(),
        "--format".to_owned(),
        "{{json .}}".to_owned(),
    ];
    if request.all {
        rest.push("--all".to_owned());
    }
    for filter in &request.filter {
        invoke::check_filter(filter)?;
        rest.push("--filter".to_owned());
        rest.push(filter.clone());
    }
    rest.extend(request.patterns.iter().cloned());

    let args = invoke::command(&request.invocation, rest)?;
    let output = invoke::run(runner, &request.invocation, &args)?;
    let command = format!("{} {}", request.invocation.bin(), args.join(" "));

    if !output.ok() && output.stdout.trim().is_empty() {
        return Ok(ImagesOutcome {
            text: format!("[no images listed: {}]\n", output.failure()),
            images: Vec::new(),
            command,
            total_size: 0,
            truncated: None,
        });
    }

    let keep_dangling = request.dangling.unwrap_or(true);
    let mut images: Vec<Image> = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|row| {
            let repository = string(&row, "Repository");
            let tag = string(&row, "Tag");
            Image {
                reference: if tag.is_empty() || tag == "<none>" {
                    repository.clone()
                } else {
                    format!("{repository}:{tag}")
                },
                id: string(&row, "ID")
                    .trim_start_matches("sha256:")
                    .chars()
                    .take(12)
                    .collect(),
                size: parse_size(&string(&row, "Size")).unwrap_or(0),
                created: string(&row, "CreatedSince"),
            }
        })
        .filter(|image| keep_dangling || !image.reference.starts_with("<none>"))
        .collect();

    let total_size: u64 = images.iter().map(|image| image.size).sum();

    match request.sort {
        Sort::Size => images.sort_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| a.reference.cmp(&b.reference))
        }),
        Sort::Created => {}
        Sort::Name => images.sort_by(|a, b| a.reference.cmp(&b.reference)),
    }

    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    let over = images.len().saturating_sub(limit);
    images.truncate(limit);

    let rows: Vec<[String; 3]> = images
        .iter()
        .map(|image| {
            [
                elide(&image.reference, 60),
                human(image.size),
                shorten(&image.created),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..2)
        .map(|column| width_of(rows.iter().map(|row| row[column].as_str())))
        .collect();

    let mut budget = crate::Budget::new(request.max_bytes);
    let mut body = String::new();
    let mut shown = 0;
    for row in &rows {
        let line = format!(
            "{}  {}  {}\n",
            pad(&row[0], widths[0]),
            pad(&row[1], widths[1]),
            row[2]
        );
        if !budget.try_spend(&line) {
            break;
        }
        body.push_str(&line);
        shown += 1;
    }
    let shown_size: u64 = images.iter().take(shown).map(|image| image.size).sum();
    images.truncate(shown);

    let cut = rows.len() - shown + over;
    let truncated = (cut > 0).then(|| {
        if over > 0 && shown == rows.len() {
            format!("{cut} more, over the limit of {limit}")
        } else {
            format!("{cut} not shown, byte budget reached")
        }
    });

    let mut footer = format!("[{}, {}", plural(images.len(), "image"), human(shown_size));
    if let Some(note) = &truncated {
        footer.push_str(&format!(", {note}"));
    }
    footer.push_str("]\n");

    Ok(ImagesOutcome {
        text: format!("{body}{footer}"),
        images,
        command,
        total_size,
        truncated,
    })
}

fn string(row: &serde_json::Value, key: &str) -> String {
    row.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// `3 weeks ago` → `3w`. Docker's own phrasing spends eleven characters on two.
fn shorten(created: &str) -> String {
    let created = created.trim_end_matches(" ago");
    let mut parts = created.split_whitespace();
    let (Some(count), Some(unit)) = (parts.next(), parts.next()) else {
        return created.to_owned();
    };
    let letter = match unit.trim_end_matches('s') {
        "second" => "s",
        "minute" => "m",
        "hour" => "h",
        "day" => "d",
        "week" => "w",
        "month" => "mo",
        "year" => "y",
        _ => return created.to_owned(),
    };
    format!("{count}{letter}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::ScriptedRunner;

    const CALL: &str = "docker images --format {{json .}}";

    fn row(repository: &str, tag: &str, id: &str, size: &str, created: &str) -> String {
        serde_json::json!({
            "Repository": repository, "Tag": tag, "ID": id,
            "Size": size, "CreatedSince": created,
        })
        .to_string()
            + "\n"
    }

    #[test]
    fn disk_space_is_the_question_so_the_biggest_image_is_first() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &format!(
                "{}{}{}",
                row("postgres", "17", "abc123def456", "438MB", "3 weeks ago"),
                row(
                    "ghcr.io/acme/ens",
                    "1.4",
                    "9f86d081884c",
                    "8.8GB",
                    "2 days ago"
                ),
                row("<none>", "<none>", "0011223344ff", "1.2GB", "5 months ago"),
            ),
        );
        let outcome = images(&runner, &ImagesRequest::default()).expect("images");

        assert_eq!(
            outcome.text,
            "\
ghcr.io/acme/ens:1.4  8.7G  2d
<none>                1.1G  5mo
postgres:17           438M  3w
[3 images, 10G]
"
        );
    }

    #[test]
    fn dangling_images_can_be_left_out_once_they_have_been_seen() {
        let runner = ScriptedRunner::new().with(
            CALL,
            &format!(
                "{}{}",
                row("postgres", "17", "abc123def456", "438MB", "3 weeks ago"),
                row("<none>", "<none>", "0011223344ff", "1.2GB", "5 months ago"),
            ),
        );
        let outcome = images(
            &runner,
            &ImagesRequest {
                dangling: Some(false),
                ..ImagesRequest::default()
            },
        )
        .expect("images");
        assert!(!outcome.text.contains("<none>"), "{}", outcome.text);
        assert_eq!(outcome.images.len(), 1);
    }

    #[test]
    fn a_filter_and_a_pattern_are_passed_through_in_docker_s_own_terms() {
        let runner = ScriptedRunner::new().with(
            "docker images --format {{json .}} --filter dangling=true ghcr.io/acme/*",
            "",
        );
        images(
            &runner,
            &ImagesRequest {
                patterns: vec!["ghcr.io/acme/*".to_owned()],
                filter: vec!["dangling=true".to_owned()],
                ..ImagesRequest::default()
            },
        )
        .expect("images");
        assert_eq!(
            runner.calls()[0],
            "docker images --format {{json .}} --filter dangling=true ghcr.io/acme/*"
        );
    }

    #[test]
    fn a_malformed_filter_is_refused_before_anything_runs() {
        let runner = ScriptedRunner::new();
        let err = images(
            &runner,
            &ImagesRequest {
                filter: vec!["dangling".to_owned()],
                ..ImagesRequest::default()
            },
        )
        .expect_err("refused");
        assert!(err.to_string().contains("key=value"), "{err}");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn an_empty_daemon_still_answers() {
        let runner = ScriptedRunner::new().with(CALL, "");
        let outcome = images(&runner, &ImagesRequest::default()).expect("images");
        assert_eq!(outcome.text, "[0 images, 0B]\n");
    }

    #[test]
    fn docker_s_own_ages_are_shortened_to_what_they_say() {
        assert_eq!(shorten("3 weeks ago"), "3w");
        assert_eq!(shorten("5 months ago"), "5mo");
        assert_eq!(shorten("2 days ago"), "2d");
        assert_eq!(shorten("Less than a second ago"), "Less than a second");
    }
}
