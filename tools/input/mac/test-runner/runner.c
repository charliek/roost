// Roost Test Runner: a never-rebuilt TCC anchor for Roost's Mac real-input harness.
//
// macOS grants Accessibility / Screen Recording / Input Monitoring to an app's
// code identity, and an ad-hoc signature changes on every rebuild. This app is
// built once and granted once; the harness then runs its (freely rebuilt)
// helpers as children of it, and TCC attributes a child to its responsible
// app. Launch through LaunchServices so this app, not the ssh session, is the
// responsible process:
//
//   open -W -n "$HOME/Applications/Roost Test Runner.app" --args <outdir> <cmd> [args...]
//
// <outdir>/stdout and <outdir>/stderr get the child's output, <outdir>/status its
// exit code (128+signal when killed). Rebuilding this app voids the grants.
#include <fcntl.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc < 3) {
        return 64;
    }
    const char *out = argv[1];
    char path[4096];
    posix_spawn_file_actions_t actions;
    posix_spawn_file_actions_init(&actions);
    posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0);
    snprintf(path, sizeof path, "%s/stdout", out);
    posix_spawn_file_actions_addopen(&actions, 1, path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    snprintf(path, sizeof path, "%s/stderr", out);
    posix_spawn_file_actions_addopen(&actions, 2, path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    pid_t pid;
    int code = 127;
    if (posix_spawnp(&pid, argv[2], &actions, NULL, argv + 2, environ) == 0) {
        int status = 0;
        while (waitpid(pid, &status, 0) < 0) {
        }
        code = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
    }
    snprintf(path, sizeof path, "%s/status", out);
    FILE *f = fopen(path, "w");
    if (f) {
        fprintf(f, "%d\n", code);
        fclose(f);
    }
    return code;
}
