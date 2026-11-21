# Stride RPM repository

Signed Stride packages for Fedora and Nobara 44 on x86-64.

To enable the repository with DNF 5:

```sh
sudo dnf config-manager addrepo --from-repofile=https://wuild.github.io/Stride/stride.repo
sudo dnf install stride
```

Once the repository is enabled, `sudo dnf upgrade` includes Stride when a newer
version is available. DNF verifies both RPM signatures and repository metadata.

Signing key fingerprint: `15468090849900177FABB8D814F8BDD786AE6595`
