#!/usr/bin/env bash
set -euo pipefail

feed=$1
asset=$2
version=$3
fingerprint=15468090849900177FABB8D814F8BDD786AE6595
channel="$feed/fedora/44/x86_64"
destination="$channel/stride-$version-1.fc44.x86_64.rpm"

if [[ $(rpm -qp --qf '%{NAME} %{VERSION} %{RELEASE} %{ARCH}' "$asset") != "stride $version 1.fc44 x86_64" ]]; then
    printf 'The RPM does not match the Stride release.\n' >&2
    exit 1
fi

export GNUPGHOME
GNUPGHOME=$(mktemp -d)
trap 'rm -rf -- "$GNUPGHOME"' EXIT
printf '%s\n' "$STRIDE_RPM_SIGNING_KEY" | gpg --batch --import
unset STRIDE_RPM_SIGNING_KEY
gpg --batch --import "$feed/RPM-GPG-KEY-stride"
gpg --batch --armor --export "$fingerprint" | cmp - "$feed/RPM-GPG-KEY-stride"
rpm --dbpath "$GNUPGHOME/rpmdb" --import "$feed/RPM-GPG-KEY-stride"

signed="$GNUPGHOME/${asset##*/}"
cp -- "$asset" "$signed"
rpmsign --define "_openpgp_sign_id $fingerprint" --addsign "$signed"
rpm --dbpath "$GNUPGHOME/rpmdb" --checksig "$signed"

if [[ -f $destination ]]; then
    if [[ $(rpm -qp --qf '%{SHA256HEADER}' "$asset") != $(rpm -qp --qf '%{SHA256HEADER}' "$destination") ]]; then
        printf 'This package version is already published with different content.\n' >&2
        exit 1
    fi
    rpm --dbpath "$GNUPGHOME/rpmdb" --checksig "$destination"
else
    install -m 0644 "$signed" "$destination"
    createrepo_c --update --retain-old-md 1 "$channel"
    gpg --batch --yes --armor --local-user "$fingerprint" --detach-sign "$channel/repodata/repomd.xml"
fi
gpg --batch --verify "$channel/repodata/repomd.xml.asc" "$channel/repodata/repomd.xml"
