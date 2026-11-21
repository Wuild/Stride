Name:           stride
Version:        %{stride_version}
Release:        1%{?dist}
Summary:        GTK 4 terminal emulator with shell suggestions
License:        LicenseRef-All-Rights-Reserved
URL:            https://github.com/Wuild/Stride
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  desktop-file-utils
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1) >= 1.6
BuildRequires:  pkgconfig(pangocairo)
Requires:       bash
Requires:       libadwaita >= 1.6

%description
Stride is a Linux terminal emulator with command suggestions, tabs, split panes,
clickable directory breadcrumbs, and persistent Bash history.

%prep
%autosetup

%build
cargo build --release --locked %{?stride_offline:--offline}

%check
cargo test --locked %{?stride_offline:--offline}
desktop-file-validate packaging/io.stride.Terminal.desktop

%install
install -Dpm 0755 "${CARGO_TARGET_DIR:-target}/release/stride-terminal" %{buildroot}%{_bindir}/stride
desktop-file-install --dir=%{buildroot}%{_datadir}/applications packaging/io.stride.Terminal.desktop
install -Dpm 0644 packaging/io.stride.Terminal.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.stride.Terminal.svg

%files
%doc README.md
%{_bindir}/stride
%{_datadir}/applications/io.stride.Terminal.desktop
%{_datadir}/icons/hicolor/scalable/apps/io.stride.Terminal.svg
