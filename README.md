# herdr-linear-agent

[![CI](https://github.com/civitaspo/herdr-linear-agent/actions/workflows/pull_request.yml/badge.svg)](https://github.com/civitaspo/herdr-linear-agent/actions/workflows/pull_request.yml)

herdr-linear-agent is a [Herdr](https://github.com/herdrdev/herdr) plugin that picks up Linear issues delegated to its Linear app user and works on them with AI coding agents running in Herdr panes.

For each issue, the plugin starts one coordinator agent. The coordinator reads the issue, splits the work, and starts one worker agent per repository in its own Herdr worktree. Workers implement the change and open pull requests. The plugin reports progress, questions, and results to the issue's Linear Agent Session, and people reply there.

## Status

The repository is being set up. No release is available yet.

## License

herdr-linear-agent is licensed under the MIT License. See [LICENSE](LICENSE).
