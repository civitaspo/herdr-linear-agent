# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.3] - 2026-10-27


### Documentation

- release preparation updates the configured version files


### Maintenance

- sync managed files
- sync managed files
- update dependency aqua:suzuki-shunsuke/ghalint to v1.5.7
- enable native Renovate merge requests

## [0.8.2] - 2026-10-25


### Maintenance

- migrate automation to csm-ci (#200)
- migrate automation to csm-ci (#199)
- update dependency github:github/gh-actions-lock to v0.4.0 (#193)
- update civitaspo/securefix-server action to v0.2.46 (#172)
- update rust crate uuid to v1.29.0 (#192)
- update dependency jdx/mise to v2026.10.18 (#191)
- update dependency github:github/gh-actions-lock to v0.3.0 (#190)
- update dependency aqua:suzuki-shunsuke/pinact to v5.0.1 (#189)
- update rust crate tokio to v1.54.1 (#188)
- update jdx/mise-action action to v5.2.2 (#187)
- update dependency jdx/mise to v2026.10.17 (#185)


### Miscellaneous

- chore(release): v0.8.2

## [0.8.1] - 2026-10-21


### Bug Fixes

- request merges through Securefix comments (#174)
- synchronize descendant cancellation test (#168)


### Maintenance

- update Securefix caller workflows to v0.2.44 (#184)
- update jdx/mise-action action to v5.2.1 (#182)
- update rust crate tokio to v1.54.0 (#183)
- update dependency jdx/mise to v2026.10.15 (#181)
- update Securefix caller workflows to v0.2.36 (#180)
- adopt GitHub Actions workflow lockfiles (#179)
- update dependency jdx/mise to v2026.10.14 (#178)
- lock file maintenance (#177)
- update Securefix caller workflows to v0.2.33 (#176)
- update dependency jdx/mise to v2026.10.13 (#170)
- update jdx/mise-action action to v5.2.0 (#169)
- update rust crate libc to v0.2.191 (#171)
- update Securefix caller workflows to v0.2.32 (#175)
- update Securefix caller workflows to v0.2.26 (#173)
- update Securefix caller workflows to v0.2.20 (#167)
- update Securefix caller workflows to v0.2.19 (#165)

## [0.8.0] - 2026-10-16


### Bug Fixes

- update rust crate rusqlite to 0.40 (#161)
- recover detached runs with comment approval (#138)
- respect queued prompts and complete coordinator turns (#115)
- restore the pull request number for workspace labels (#109)
- identify tickets in agent workspace labels (#108)


### Documentation

- explain the historical PR number repair (#113)
- document repository development workflow (#101)


### Features

- keep issue agents in one Herdr workspace (#163)
- add postmortem supervisor (#157)
- add optional merge decision agent (#151)
- restore the factory workflow and authorize PR merges (#146)
- add shared runs and authorized workflow completion (#144)


### Maintenance

- update dependency rust to v1.99.1 (#162)
- update dependency jdx/mise to v2026.10.11 (#160)
- update Securefix caller workflows to v0.2.18 (#159)
- update Securefix caller workflows to v0.2.17 (#158)
- update Securefix caller workflows to v0.2.16 (#155)
- update dependency jdx/mise to v2026.10.10 (#154)
- update Securefix caller workflows to v0.2.15 (#153)
- update Securefix caller workflows to v0.2.14 (#152)
- update rust crate tokio to v1.53.3 (#150)
- update dependency jdx/mise to v2026.10.9 (#149)
- update actions/create-github-app-token action to v3.3.0 (#148)
- update rust crate sha2 to v0.11.1 (#147)
- update jdx/mise-action action to v5.1.2 (#143)
- update rust crate serde_json to v1.0.154 (#139)
- update dependency jdx/mise to v2026.10.8 (#136)
- update Securefix caller workflows to v0.2.12 (#137)
- update Securefix caller workflows to v0.2.11 (#135)
- update Securefix caller workflows to v0.2.10 (#134)
- update rust crate serde_json to v1.0.152 (#130)
- update Securefix caller workflows to v0.2.9 (#133)
- update Securefix caller workflows to v0.2.8 (#132)
- update rust crate toml to v1.1.8 (#116)
- update Securefix caller workflows to v0.2.7 (#129)
- update Securefix caller workflows to v0.2.6 (#128)
- update Securefix caller workflows (#127)
- update Securefix caller workflows (#126)
- update dependency jdx/mise to v2026.10.7 (#114)
- update rust crate uuid to v1.28.0 (#124)
- update Securefix caller workflows (#125)
- update Securefix caller workflows (#123)
- update Securefix caller workflows (#122)
- update Securefix caller workflows (#121)
- update Securefix caller workflows (#118)
- update rust crate toml to v1.1.7 (#112)
- route human merge requests through Securefix (#110)
- update dependency jdx/mise to v2026.10.4 (#111)
- update rust crate zeroize to v1.9.1 (#107)
- update rust crate jiff to v0.2.38 (#106)
- update dependency jdx/mise to v2026.10.3 (#105)
- update jdx/mise-action action to v5.1.1 (#104)
- update dependency jdx/mise to v2026.10.2 (#103)


### Miscellaneous

- undo merged PR #144 (#145)

## [0.7.1] - 2026-10-04


### Bug Fixes

- open pull request links in Linear review (#100)


### Maintenance

- update jdx/mise-action action to v5.1.0 (#99)
- update dependency jdx/mise to v2026.10.1 (#98)
- update rust crate tokio to v1.53.2 (#97)
- sign local macOS development builds (#96)
- update rust crate uuid to v1.27.0 (#94)

## [0.7.0] - 2026-10-03


### Bug Fixes

- resume crashed agents with a persistent retry limit (#92)
- preserve native Linear input while agents report progress (#91)
- protect persisted state and recover failed worker launches (#86)


### Features

- show worker progress and the coordinator's reasoning in Linear (#90)


### Maintenance

- update rust crate libc to v0.2.190 (#93)
- update dependency jdx/mise to v2026.10.0 (#89)
- update dependency rust to v1.99.0 (#88)

## [0.6.0] - 2026-10-01


### Features

- spell out herdr-linear-agent in pane tokens and other names (#85)
- pick routings per team and postmortems when they are written (#82)


### Maintenance

- update jdx/mise-action action to v5.0.1 (#84)

## [0.5.0] - 2026-09-30


### Bug Fixes

- begin each postmortem with how the run stands (#81)
- tell the final postmortem which state the issue closed in (#80)
- color the history screens with the theme's palette (#78)


### Features

- post interim and final postmortems per team (#79)
- record agent sessions and keep their transcripts in the run folder (#76)
- browse past runs and their agent transcripts (#74)


### Maintenance

- update dependency jdx/mise to v2026.9.18 (#77)

## [0.4.0] - 2026-09-30


### Bug Fixes

- take the delegator from the latest delegation in the issue history (#73)
- end the Linear session when a run closes (#68)


### Documentation

- record the live check of allowed delegators (#72)
- record the live check of the closing response and manual reload (#70)
- tell users to disable the webhook's delivery (#67)
- record the live check of profile inheritance and config reload (#66)
- record the live check with two Linear workspaces (#63)
- record the live check of workspace-qualified runs (#62)
- record that no Linear scope or mutation exposes a worker's checks (#59)
- record that the app user cannot read a worker's checks from Linear (#58)
- record which Linear fields carry pull request checks and reviews (#57)


### Features

- take only issues delegated by allowed users (#71)
- reload the config on request and tell when it changed (#69)
- reload the config without restarting the ticker (#65)
- let a profile inherit from another profile (#64)
- support several Linear workspaces and per-team settings (#61)
- read each profile from a folder of its own (#60)
- keep coordinators off their own subagents and workers off CI polling (#56)


### Maintenance

- update dependency jdx/mise to v2026.9.17 (#49)
- update jdx/mise-action action to v5 (#50)

## [0.3.0] - 2026-09-29


### Documentation

- record the Cursor Agent and OpenCode coordinator checks (#52)
- record the live routing check in the scratch Linear team (#48)


### Features

- let the routing agent's profile set environment variables (#53)
- support Cursor Agent and OpenCode as coordinators and routing agents (#51)
- let a routing agent pick each issue's coordinator profile (#46)


### Maintenance

- update mise to 2026.9.16 (#54)

## [0.2.1] - 2026-09-28


### Bug Fixes

- update rust crate toml to v1 (#15)


### Maintenance

- keep reqwest on the version oauth2 uses (#45)

## [0.2.0] - 2026-09-28


### Bug Fixes

- leave agent session comments out of the issue hash (#42)
- keep a just-prompted worker working and open worktrees with their checkout (#40)
- recognize a coordinator Herdr is still launching (#37)


### Documentation

- record the live check of issue edits (#43)
- record the integration test of the async rewrite (#41)
- record the behavior the async rewrite keeps (#31)


### Features

- pace Linear reads by the rate-limit budget (#38)
- make the ticker event-driven on the Herdr socket (#35)
- run on tokio and read Herdr over its socket (#32)


### Maintenance

- lock file maintenance (#34)


### Refactor

- rewrite the remaining helpers and drop the herdr-projects attribution (#39)

## [0.1.0] - 2026-09-27


### Bug Fixes

- hold prompts and heartbeats after a stop until the next reply (#28)
- use the session Linear creates on delegation and finish cut-short claims (#24)
- accept Linear's space-separated scope and report the OAuth step that failed (#23)
- give the link handler the title Herdr requires (#22)
- update rust crate sha2 to 0.11 (#13)
- update rust crate base64 to 0.23 (#11)


### Documentation

- record the first integration test (#29)
- tell herdr-linear-agent apart from Linear's Linear Agent (#21)
- explain how worker brief folders stay out of commits (#20)


### Features

- optionally pre-trust Claude Code folders before launching agents (#25)
- support Linux (#16)
- implement the Linear agent plugin (#8)


### Maintenance

- update dependency jdx/mise to v2026.9.15 (#27)
- update dependency jdx/mise to v2026.9.14 (#14)
- update dependency aqua:suzuki-shunsuke/pinact to v5 (#5)
- update csm-actions/approve-pr-action action to v1 (#3)
- drop the bootstrap guards from tasks and CI (#19)
- remove a timing assumption from the routing agent scenario (#18)
- attach macOS binaries to releases (#9)
- update dependency jdx/mise to v2026.9.14 (#12)
- pin rust crate tempfile to =3.27.0 (#10)
- add release workflows (#4)
- update dependency rust to v1.98.1 (#2)
- repository foundation (#1)
- bootstrap repository


### Refactor

- rename claude.pre_trust to claude.auto_accept_trust_dialog (#26)


