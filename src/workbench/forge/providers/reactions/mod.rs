//! Set or confirm the current viewer's reaction, scoped to one verified issue or comment.
use super::*;
mod github;
mod linear;
mod rest;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Content {
    ThumbsUp,
    ThumbsDown,
    Laugh,
    Hooray,
    Confused,
    Heart,
    Rocket,
    Eyes,
}
impl Content {
    pub(super) fn from_linear(value: &str) -> Option<Self> {
        [
            Self::ThumbsUp,
            Self::ThumbsDown,
            Self::Laugh,
            Self::Hooray,
            Self::Confused,
            Self::Heart,
            Self::Rocket,
            Self::Eyes,
        ]
        .into_iter()
        .find(|c| c.linear() == value || c.linear_name() == value)
    }
    fn standard(self) -> &'static str {
        [
            "+1", "-1", "laugh", "hooray", "confused", "heart", "rocket", "eyes",
        ][self as usize]
    }
    fn github(self) -> &'static str {
        [
            "THUMBS_UP",
            "THUMBS_DOWN",
            "LAUGH",
            "HOORAY",
            "CONFUSED",
            "HEART",
            "ROCKET",
            "EYES",
        ][self as usize]
    }
    fn gitlab(self) -> &'static str {
        [
            "thumbsup",
            "thumbsdown",
            "laughing",
            "tada",
            "confused",
            "heart",
            "rocket",
            "eyes",
        ][self as usize]
    }
    fn linear(self) -> &'static str {
        ["👍", "👎", "😄", "🎉", "😕", "❤️", "🚀", "👀"][self as usize]
    }
    fn linear_name(self) -> &'static str {
        [
            "+1", "-1", "smile", "tada", "confused", "heart", "rocket", "eyes",
        ][self as usize]
    }
    pub(super) fn azure(self) -> Option<&'static str> {
        ["like", "dislike", "smile", "hooray", "confused", "heart"]
            .get(self as usize)
            .copied()
    }
}
fn positive(id: &str) -> Result<u64, ForgeError> {
    let n = id
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == id);
    n.ok_or_else(|| ForgeError::bad("invalid native comment ID"))
}
pub(super) fn satisfied(current: bool, want: bool, write: bool) -> Result<bool, ForgeError> {
    if current == want {
        Ok(true)
    } else if write {
        Ok(false)
    } else {
        Err(ForgeError::uncertain(
            "the selected subject does not confirm the requested viewer reaction",
        ))
    }
}
pub(in crate::workbench::forge) async fn execute(
    c: &Context,
    op: &Operation,
    write: bool,
) -> Result<Value, ForgeError> {
    let Operation::SetIssueReaction {
        number,
        subject_id,
        content,
        reacted,
    } = op
    else {
        unreachable!()
    };
    let (n, subject, content, want) = (*number, subject_id.as_deref(), *content, *reacted);
    match c.kind.as_str() {
        "github" => github::apply(c, n, subject, content, want, write).await?,
        "gitlab" | "forgejo" => rest::apply(c, n, subject, content, want, write).await?,
        "linear" => linear::apply(c, n, subject, content, want, write).await?,
        "azure-devops" => super::azure::reaction(c, n, subject, content, want, write).await?,
        _ => {
            return Err(ForgeError::unsupported(
                "this tracker has no issue reactions",
            ));
        }
    }
    Ok(json!({"number":n,"subject_id":subject,"content":content,"reacted":want}))
}

/// Hold the credential used for both the identity check and the subsequent reaction request.
pub(in crate::workbench::forge) async fn viewer(c: &mut Context) -> Result<String, ForgeError> {
    if let Some(token) = super::credentials::resolve(c).await? {
        c.token = Some(token)
    }
    let id = match c.kind.as_str() {
        "github" | "gitlab" | "forgejo" => get(c, path(c, &["user"])?).await?["id"]
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| n.to_string()),
        "linear" => super::linear::gql(
            c,
            "query KarReactionViewer { viewer { id } }",
            json!({}),
            false,
        )
        .await?["viewer"]["id"]
            .as_str()
            .map(str::to_owned),
        "azure-devops" => Some(super::azure::viewer_id(c).await?),
        _ => {
            return Err(ForgeError::unsupported(
                "this tracker has no issue reactions",
            ));
        }
    };
    id.filter(|id| !id.is_empty())
        .ok_or_else(|| ForgeError::remote(401, "tracker did not identify the authenticated user"))
}
