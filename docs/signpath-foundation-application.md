# SignPath Foundation Application: CPU Manager

Use the text below when submitting the SignPath Foundation application. Replace
bracketed values only when they differ from the repository's public details.

## Application Text

**Project name:** CPU Manager (CPUM)

**Project URL:** https://github.com/yixinin/cpum

**Primary maintainer:** Eason (GitHub: `yixinin`, email:
`yixinin@outlook.com`)

**Project description:**

CPU Manager is a free and open-source Windows desktop application for viewing
CPU topology and managing per-process CPU affinity. It lets a local computer
user inspect logical processors, physical cores, SMT threads, packages, NUMA
topology, and detected CCD/die information; monitor process resource use; set
process affinity masks; and create persistent process-name affinity rules. An
optional Windows service can apply saved local affinity rules automatically.

The project is built with Rust, Tauri, Vue 3, and TypeScript. It is intended
for local system administration and performance tuning.

**Open-source status and license:**

The complete source code is publicly available at the Project URL above under
the MIT License:
https://github.com/yixinin/cpum/blob/main/LICENSE

**Why code signing is needed:**

CPU Manager is distributed as a Windows NSIS installer. It includes a desktop
application that reads process and CPU topology data through Windows APIs and,
when requested by the user, changes process affinity. Some operations need
elevation depending on the target process. The optional service is installed
per machine as `CpumAffinityService` and applies affinity rules stored on the
local computer. Authenticode signing lets users verify that an installer from
the project's GitHub Releases was produced by this project and was not
modified after release.

**Requested signing scope:**

- Windows x64 NSIS installer: `CPU Manager_<version>_x64-setup.exe`
- Windows x64 desktop application: `cpum.exe`
- Windows x64 optional affinity service: `cpum_service.exe`

Only artifacts built from the public `yixinin/cpum` repository by its GitHub
Actions release workflow should be eligible for signing.

**Build and release provenance:**

Releases are built on GitHub-hosted `windows-latest` runners. A maintainer
pushes a version tag matching `v*`; the repository workflow at
`.github/workflows/release.yml` installs dependencies, runs the Tauri release
build, and creates the GitHub Release. When SignPath credentials are configured
as GitHub Actions secrets, it also submits the x64 and ARM64 installers to
SignPath and verifies the returned Authenticode signatures. Source, workflow,
release tag, and published binaries remain publicly auditable.

## Recommended SignPath Policy

Configure the project policy to permit signing requests only when all of the
following apply:

- Request origin is the GitHub Actions workflow for `yixinin/cpum`.
- Artifact source is a protected release tag matching `v*`.
- The request uses the designated `release.yml` workflow.
- The artifact configuration accepts only the Windows executables named above.
- A GitHub Release is created for the matching tag after successful signing.

Do not grant the GitHub Actions token permission to sign arbitrary local files
or artifacts from forks.

## Information to Provide Outside This Document

SignPath may require details that cannot safely live in the repository. Supply
these directly in its web form or secure portal:

- Legal name and contact information for the responsible maintainer.
- Confirmation that you control the `yixinin` GitHub account and repository.
- Any requested confirmation of the project's open-source eligibility.
- The SignPath-generated API token and project/policy identifiers. Store those
  only as GitHub Actions secrets; never commit them or include them in an issue
  or pull request.

## Submission Checklist

- [ ] The repository is public at `https://github.com/yixinin/cpum`.
- [ ] The `LICENSE` file is present on the default branch.
- [ ] The README explains the application, its privileged operations, and its
      build process.
- [ ] A maintainer has reviewed and accepted the SignPath Foundation terms.
- [ ] The SignPath policy restricts requests to this repository's protected
      release tags.
- [ ] SignPath credentials are stored as GitHub Actions secrets.
- [ ] A test tag has completed a signed release and its installer verifies as
      `Valid` in Windows.
