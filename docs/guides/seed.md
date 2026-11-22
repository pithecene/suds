# Seeded Build State

This guide describes the `suds seed` command, which gives a new checkout the build state of another checkout of its repository.

```sh
suds seed ~/workspace/app/.worktrees/fix            # the plan, with nothing changed
suds seed ~/workspace/app/.worktrees/fix --apply    # clone what the plan names
suds seed ~/workspace/app/.worktrees/fix --from ~/workspace/app --apply
```

A new linked checkout holds no build state, so its first build compiles
each dependency again. The command clones the Cargo build state of a built
checkout into the new one. A clone shares its blocks with its source until
a build changes one of the two, so it occupies almost no space. Cargo then
compiles the workspace's own packages and takes each dependency from the
clone. [Decision 0006](../decisions/0006-seeded-build-state.md) holds the
measurement and the reasons.

Without `--apply` the command prints the plan and changes nothing, and
`--format json` writes the whole `suds.seed-plan/v1` plan. The option
`--client` names the program that asks, such as `bosun`, for the journal.

## Source

Git lists the checkouts of the destination's repository, and the plan
considers the build state of each one. It takes one checkout:

1. A checkout whose `Cargo.lock` equals the destination's comes first,
   because its build state holds the same version of each dependency.
2. The larger build state comes next.
3. The build state that changed last breaks a tie.

The option `--from` names the checkout instead.

## Conditions

The plan clones a directory only while each of these holds, and the run
checks each one again:

- The source is Cargo's build state by its layout: a `target` directory
  with Cargo's tag, beside a `Cargo.toml`.
- The destination's path holds nothing. A seed changes no existing entry.
- The destination's directory holds a `Cargo.toml`, so a build there would
  use the clone.
- Each directory between the destination's checkout and that path is a
  plain directory. A seed follows no symbolic link.
- The source holds no checkout and no mount point, and Suds could read
  each entry of it. A checkout holds work that no build regenerates.
- Both checkouts lie on one filesystem.
- No build holds the source. Cargo locks a file of each profile's
  directory for a whole build, and Suds takes that lock in shared form
  for the clone's duration.

The plan lists each checkout and each directory that it passes, with the
reason.

## Clone

The run clones under another name beside the destination and renames the
result once it is whole, so a build never meets a part of a clone. It
keeps each file's time, each hard link, and each directory's permissions.

A seed never copies. On a filesystem that clones no file, such as ext4,
the run writes nothing and states the reason. Btrfs and XFS clone on
Linux, and APFS clones on macOS.

## Limits

- The command seeds the build state of Cargo alone.
- On Linux, a measurement counts a cloned block once, in the first file
  that the scan reaches. A file below 16 KiB counts in full. On macOS, a
  measurement counts a cloned file in each checkout that holds it, so the
  reported usage exceeds the space that the disk holds for clones.
- Cargo compiles the workspace's own packages again after a seed, because
  the new checkout's source files are newer than the cloned artifacts.
