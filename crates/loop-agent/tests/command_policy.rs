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
