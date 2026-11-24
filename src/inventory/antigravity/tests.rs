use super::*;
use serde_json::json;
use std::fs;

fn write(path: &Path, data: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}
fn skill(dir: &Path, header: &str) {
    write(
        &dir.join("SKILL.md"),
        &format!("---\n{header}\n---\nbody\n"),
    );
}
#[tokio::test]
async fn aliases_manifests_hidden_duplicates_and_gitfile_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let sub = repo.join("sub");
    write(&repo.join(".git"), "gitdir: ../git-store\n");
    write(
        &temp.path().join("git-store/HEAD"),
        "ref: refs/heads/test\n",
    );
    skill(
        &repo.join(".agent/skills/duplicate"),
        "name: winner\ndescription: repo",
    );
    skill(
        &sub.join(".agents/skills/duplicate"),
        "name: winner\ndescription: sub",
    );
    skill(
        &repo.join(".agent/skills/hidden"),
        "name: hidden\ndisable-slash-command: y",
    );
    skill(&sub.join(".agents/skills/hidden"), "name: hidden");
    let extras = repo.join("extras");
    skill(
        &extras.join("deep/nested"),
        "name: 1.0\ndescription: 2026-11-18",
    );
    skill(&extras.join("other"), "name: other");
    skill(&home.join("outside"), "name: outside");
    write(
        &sub.join(".agents/skills.json"),
        &json!({"inherits":[{"path":"inherited.json","include_only":["nested"]}]}).to_string(),
    );
    write(&repo.join("inherited.json"),&json!({"entries":[{"path":"extras","include_only":["deep/nested","other"]},{"path":home}],"inherits":[{"path":"sub/.agents/skills.json"}]}).to_string());
    let plugin = sub.join(".agents/plugins/demo");
    write(
        &plugin.join(".antigravity-plugin/plugin.json"),
        r#"{"name":"My plugin","disabled":true}"#,
    );
    skill(
        &plugin.join("skills/cmd"),
        "{ name: cmd, disable-model-invocation: yes }",
    );
    let duplicate = sub.join(".agents/plugins/z-copy");
    write(&duplicate.join("plugin.json"), "{}");
    skill(
        &duplicate.join("skills/cmd"),
        "{ name: cmd, disable-model-invocation: yes }",
    );
    write(
        &home.join(".gemini/config/config.json"),
        r#"{"plugins":{"demo":{"enabled":true}}}"#,
    );
    skill(
        &home.join(".gemini/config/skills/bad"),
        "disable-model-invocation: \"true\"",
    );
    skill(
        &home.join(".gemini/config/skills/bad-duplicate"),
        "name: one\nname: two",
    );
    skill(
        &home.join(".gemini/config/skills/null-name"),
        "name: null\ndescription: |-\n  1\n  2",
    );
    skill(
        &home.join(".gemini/antigravity-cli/builtin/skills/native_swarm"),
        "name: swarm",
    );
    skill(
        &home.join(".gemini/antigravity-cli/builtin/skills/automation"),
        "name: automation",
    );
    write(
        &home.join(".gemini/antigravity-cli/skills.json"),
        r#"{"entries":[{"path":"legacy"}]}"#,
    );
    write(
        &home.join(".gemini/config/skills.json"),
        r#"{"entries":[{"path":"new"}]}"#,
    );
    for name in ["legacy", "new"] {
        skill(
            &home.join(".gemini/config").join(name).join("child"),
            &format!("name: {name}"),
        );
    }
    let found = files::discover(&sub, &home).await.unwrap();
    assert_eq!(
        found.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        [
            "1.0",
            "automation",
            "demo:cmd",
            "legacy",
            "null-name",
            "winner"
        ]
    );
    assert_eq!(found[0].description.as_deref(), Some("2026-11-18"));
    assert_eq!(found[1].scope, Some(SkillScope::Builtin));
    assert_eq!(found[2].disable_model_invocation, Some(true));
    assert_eq!(found[2].plugin.as_deref(), Some("My plugin"));
    assert_eq!(found[4].description.as_deref(), Some("1\n2"));
    assert_eq!(found[5].description.as_deref(), Some("repo"));
}
#[cfg(unix)]
#[tokio::test]
async fn symlinks_in_project_user_and_inherited_manifests() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    let home = temp.path().join("home");
    let outside = temp.path().join("outside/skill");
    skill(&outside, "name: external");
    let skills = cwd.join(".agents/skills");
    fs::create_dir_all(skills.join("file")).unwrap();
    symlink(&outside, skills.join("dir")).unwrap();
    symlink(outside.join("SKILL.md"), skills.join("file/SKILL.md")).unwrap();
    let good = cwd.join("internal/skill");
    skill(&good, "name: inside");
    symlink(&good, skills.join("good")).unwrap();
    write(
        &cwd.join(".agents/skills.json"),
        r#"{"entries":[{"path":".","include_only":["../outside/skill"]}]}"#,
    );
    let user = home.join(".gemini/config/skills");
    fs::create_dir_all(&user).unwrap();
    symlink(&outside, user.join("external")).unwrap();
    let found = files::discover(&cwd, &home).await.unwrap();
    assert_eq!(
        found.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["external", "inside"]
    );
    assert!(
        found[0]
            .path
            .as_ref()
            .unwrap()
            .contains(".gemini/config/skills")
    );
    assert!(
        found[1]
            .path
            .as_ref()
            .unwrap()
            .contains(".agents/skills/good/")
    );
}
