use help_test::HelpTest;

#[test]
fn help_examples() {
    HelpTest::new("dotsync")
        .allow_short_flags(&["m"])
        .page(&[], |fixture| {
            fixture.env("HOME", ".");
            fixture.env("DOTSYNC_OS", "linux");
            fixture.env("DOTSYNC_HOSTNAME", "mx-help-test");
            fixture.command("git", &["init", "--bare", "<url>"]);
            fixture.command("git", &["init", "--bare", "remote.git"]);
        })
        .example(&[], &[], |fixture| {
            fixture.command(env!("CARGO_BIN_EXE_dotsync"), &["init", "remote.git"]);
        })
        .example(
            &[],
            &["commit", "linux", "-m", "add bashrc", ".bashrc"],
            |fixture| {
                fixture.command(env!("CARGO_BIN_EXE_dotsync"), &["init", "remote.git"]);
                fixture.command(
                    "sh",
                    &[
                        "-lc",
                        "printf 'export PATH=\"$HOME/bin:$PATH\"\\n' > .bashrc",
                    ],
                );
            },
        )
        .page(&["init"], |fixture| {
            fixture.env("HOME", ".");
            fixture.env("DOTSYNC_OS", "linux");
            fixture.env("DOTSYNC_HOSTNAME", "mx-help-test");
            fixture.command("git", &["init", "--bare", "<url>"]);
        })
        .page(&["commit"], |_fixture| {})
        .page(&["continue"], |_fixture| {})
        .page(&["abort"], |_fixture| {})
        .page(&["status"], |_fixture| {})
        .run();
}
