//! The bash safety block list reads the command as the shell runs it: it
//! blocks recursive `rm`/`chmod`/`chown` only on root or home targets.

use loop_agent::harness::tools::check_command_policy;

#[test]
fn recursive_commands_on_ordinary_paths_run() {
    for cmd in [
        "chmod -R u+w /work/project",
        "chmod -R 777 ./x",
        "chmod -R 777 /work/project",
        "rm -rf /tmp/x",
        "rm -rf ./build",
        "rm -rf build && cargo test",
        "chown -R me:staff ./src",
        "rm -r /home/user/project/target",
        "git commit -m 'rm -rf /'",
        "sh -c 'rm -rf ./build'",
        "bash -c \"chmod -R u+w /work/project\"",
    ] {
        assert!(
            check_command_policy(cmd, &[]).is_ok(),
            "`{cmd}` should run: {:?}",
            check_command_policy(cmd, &[])
        );
    }
}

#[test]
fn recursive_commands_on_root_or_home_stay_blocked() {
    for cmd in [
        "rm -rf /",
        "rm -rf /*",
        "rm -rf ~",
        "rm -rf ~/",
        "rm -rf $HOME",
        "rm -rf \"$HOME\"/",
        "sudo rm -rf /",
        "rm -rf -- /",
        "rm -fr //",
        "rm --recursive --force /",
        "/bin/rm -rf /",
        "chmod -R 777 /",
        "chown -R nobody /",
        "cd /tmp && rm -rf /",
        "rm -rf '/'",
        "r\\m -rf /",
        "RM -RF /",
        "sh -c 'rm -rf /'",
        "bash -c \"rm -rf ~\"",
        "bash -lc 'chmod -R 777 /'",
        "eval 'rm -rf /'",
        "sudo sh -c 'cd /tmp && rm -rf /'",
        "sh -c 'mkfs.ext4 /dev/sda1'",
    ] {
        assert!(
            check_command_policy(cmd, &[]).is_err(),
            "`{cmd}` should be blocked"
        );
    }
}

#[test]
fn other_default_patterns_stay_blocked() {
    for cmd in [
        "mkfs.ext4 /dev/sda1",
        ":(){ :|:& };:",
        "echo x > /dev/sda",
        "dd if=/dev/zero of=/dev/sda",
        "format c:",
    ] {
        assert!(
            check_command_policy(cmd, &[]).is_err(),
            "`{cmd}` should be blocked"
        );
    }
}

#[test]
fn system_directories_dot_dot_and_globs_stay_blocked() {
    for cmd in [
        "rm -rf /usr",
        "rm -rf /etc",
        "rm -rf /home",
        "rm -rf /Users",
        "rm -rf /System",
        "rm -rf /usr/",
        "chmod -R 777 /etc",
        "rm -rf /..",
        "rm -rf ~/..",
        "rm -rf $HOME/..",
        "rm -rf /tmp/../",
        "rm -rf /usr/local/..",
        "rm -rf /{,}",
        "rm -rf /**",
        "rm -rf /?*",
        "rm -rf /u*",
        "rm -rf ~root",
        "rm -rf ~user/",
    ] {
        assert!(
            check_command_policy(cmd, &[]).is_err(),
            "`{cmd}` should be blocked"
        );
    }
}

#[test]
fn the_literal_home_path_stays_blocked() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    if home.trim_end_matches('/').is_empty() {
        return;
    }
    for cmd in [
        format!("rm -rf {home}"),
        format!("rm -rf {home}/"),
        format!("rm -rf {home}/*"),
    ] {
        assert!(
            check_command_policy(&cmd, &[]).is_err(),
            "`{cmd}` should be blocked"
        );
    }
    let inside = format!("rm -rf {home}/project/target");
    assert!(
        check_command_policy(&inside, &[]).is_ok(),
        "`{inside}` should run"
    );
}

#[test]
fn scripts_piped_or_fed_to_a_shell_stay_blocked() {
    for cmd in [
        "echo 'rm -rf /' | sh",
        "printf 'rm -rf ~' | bash",
        "sh <<< 'rm -rf /'",
        "bash <<<'rm -rf /'",
        "su -c 'rm -rf /'",
        "su root -c 'chmod -R 777 /'",
        "ssh host 'rm -rf /'",
        "ssh -p 22 host rm -rf /",
    ] {
        assert!(
            check_command_policy(cmd, &[]).is_err(),
            "`{cmd}` should be blocked"
        );
    }
}

#[test]
fn nesting_deeper_than_the_limit_fails_closed() {
    let mut cmd = "echo hi".to_owned();
    for _ in 0..8 {
        cmd = format!("sh -c {}", shell_quote(&cmd));
    }
    assert!(
        check_command_policy(&cmd, &[]).is_err(),
        "`{cmd}` should be blocked"
    );
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[test]
fn ordinary_globs_and_pipes_still_run() {
    for cmd in [
        "rm -rf /tmp/build/*",
        "rm -rf ./target/*",
        "rm -rf ~/project/target",
        "echo hi | sh",
        "cat build.log | grep error",
        "ssh host ls -la",
        "chmod -R u+w /var/folders/x/project",
    ] {
        assert!(
            check_command_policy(cmd, &[]).is_ok(),
            "`{cmd}` should run: {:?}",
            check_command_policy(cmd, &[])
        );
    }
}
