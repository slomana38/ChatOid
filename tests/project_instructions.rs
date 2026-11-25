use anyhow::{Context, Result};
use chatoid::agent::{
    Settings,
    app_server::{TaskInput, TaskThread, list_skills, run_task},
};
use std::{fs, sync::atomic::AtomicBool};
use uuid::Uuid;

#[test]
#[ignore = "requires authenticated local Codex and a working workspace sandbox"]
fn real_codex_applies_project_instructions_and_the_selected_skill() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let workspace = fixture.path().join("work");
    fs::create_dir(&workspace)?;
    let shared_skills = fixture.path().join("shared-skills");
    let skill_root = shared_skills.join("chatoid-proof");
    fs::create_dir_all(&skill_root)?;
    let project_marker = format!("PROJECT-{}", Uuid::new_v4());
    let skill_marker = format!("SKILL-{}", Uuid::new_v4());
    fs::write(
        workspace.join("AGENTS.md"),
        format!(
            "# Project instructions\nWhen asked to create rule-proof.txt, its first line must be exactly {project_marker}. Follow the operator-selected skill for the second line. Use LF newlines, with a final newline and no other content.\n"
        ),
    )?;
    fs::write(
        skill_root.join("SKILL.md"),
        format!(
            "---\nname: chatoid-proof\ndescription: Creates the local fixture proof using the project instructions.\n---\nWhen creating rule-proof.txt, its second line must be exactly {skill_marker}. Keep the first line required by AGENTS.md.\n"
        ),
    )?;
    let settings = Settings {
        task_timeout_minutes: 2,
        skill_roots: vec![shared_skills],
        ..Settings::default()
    };
    let skills = list_skills(&settings, &fixture.path().join("skill-catalog"), &workspace)?;
    let skill = skills
        .into_iter()
        .find(|skill| skill.name == "chatoid-proof")
        .context("Fixture skill was not discovered")?;
    let result = run_task(
        &settings,
        &workspace,
        TaskInput {
            text: "Create rule-proof.txt using this project's AGENTS.md instructions and the explicitly selected chatoid-proof skill. Use your editing tool. Report what you did.",
            attachment: None,
            web_research: false,
            network: false,
            delegate: false,
            steering: None,
            model: None,
            plan_only: false,
            review: None,
            skills: &[skill],
            compact_before: false,
        },
        TaskThread::Ephemeral,
        &AtomicBool::new(false),
        None,
        |_| {},
    )?;
    let contents = fs::read_to_string(workspace.join("rule-proof.txt"))
        .with_context(|| format!("Proof was not created; Codex reported {}", result.text))?;
    assert_eq!(contents, format!("{project_marker}\n{skill_marker}\n"));
    Ok(())
}
