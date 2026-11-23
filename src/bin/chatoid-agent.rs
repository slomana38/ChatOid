#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    chatoid::desktop::run(chatoid::identity::Role::Agent)
}
