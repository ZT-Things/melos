# Packaging

Three AUR packages are prepared:

| Directory | Package | Source |
| --- | --- | --- |
| `.` | `melos` | tagged source tarball, builds with cargo |
| `melos-bin` | `melos-bin` | prebuilt x86_64 release tarball |
| `melos-git` | `melos-git` | latest git `main` |

All three install `melos` + `melosd`, the systemd user socket/service, man pages,
and bash/zsh/fish completions. The service is **not** enabled automatically.

## Before uploading

1. Create the public repository at <https://github.com/ZT-Things/melos> and push
   the source (see the release script below).
2. Create the `v0.2.0` tag and a GitHub release with
   `melos-0.2.0-x86_64-unknown-linux-gnu.tar.gz`.
3. Replace `sha256sums=('SKIP')` with real hashes:
   `cd packaging && updpkgsums`, then the same in `melos-bin/`.
4. Check with `namcap PKGBUILD` and `makepkg -si` in a clean chroot.
5. Register an AUR account, add an SSH public key at
   <https://aur.archlinux.org/account>, then push each directory as its own AUR
   repository (`ssh://aur@aur.archlinux.org/melos.git`, `melos-bin.git`,
   `melos-git.git`).

`melos-git` uses `SKIP` legitimately because it is a VCS package. The stable
packages must not be uploaded with `SKIP`.

## Local package build

```sh
cd packaging
makepkg -f
```

The source package needs the GitHub tag to exist. To test packaging before the
tag, build the workspace directly and use `DESTDIR` manually, or temporarily
point `source` at a local tarball.

## Release tarball

From the repository root:

```sh
./scripts/release.sh 0.2.0
```

This creates `dist/` with the source tarball, the prebuilt x86_64 tarball, and
`sha256sums.txt`.
