# Reference spec for the Buran Application Server, CI-verified on every
# release in a clean Fedora container. For an official Fedora submission:
# vendor the crates (rust2rpm -V) — network access below is CI-only.

%global debug_package %{nil}
# PHP module binary carries the version in its name (buran-php84), matching
# `module:` in buran.yaml one to one.
%global php_suffix %(php-config --version 2>/dev/null | awk -F. '{print $1$2}')

Name:           buran
Version:        0.1.0
Release:        1%{?dist}
Summary:        Buran universal application server

License:        Apache-2.0
URL:            https://github.com/buran-project/buran
Source0:        buran-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust >= 1.85
BuildRequires:  gcc
BuildRequires:  php-devel
BuildRequires:  php-embedded
BuildRequires:  systemd-rpm-macros

%description
High-performance application server: static files, routing/rewrites and
language runtimes (PHP via embed SAPI) behind one process tree with a
static YAML configuration.

%package php
Summary:        PHP runtime module for %{name}
Requires:       %{name} = %{version}-%{release}
Requires:       php-embedded

%description php
PHP runtime module for Buran, linked against the PHP version shipped by
this distribution release.

%prep
%autosetup

%build
cargo build --release --locked -p buran
# Fedora ships libphp.so in %%{_libdir} (lib64), which php-config's prefix
# does not reveal — point the build there explicitly.
BURAN_PHP_LIB_DIR=%{_libdir} cargo build --release --locked -p buran-php

%install
install -Dm755 target/release/buran %{buildroot}%{_sbindir}/buran
install -Dm644 packaging/common/buran/buran.yaml %{buildroot}%{_sysconfdir}/buran/buran.yaml
install -dm755 %{buildroot}/usr/lib/buran/modules
install -Dm755 target/release/buran-php \
    %{buildroot}/usr/lib/buran/modules/buran-php%{php_suffix}
install -Dm644 packaging/common/systemd/buran.service %{buildroot}%{_unitdir}/buran.service
install -Dm644 packaging/common/systemd/buran.sysusers.conf %{buildroot}%{_sysusersdir}/buran.conf

%pre
%sysusers_create_compat packaging/common/systemd/buran.sysusers.conf

%post
%systemd_post buran.service

%preun
%systemd_preun buran.service

%postun
%systemd_postun_with_restart buran.service

%files
%license LICENSE
%{_sbindir}/buran
%dir %{_sysconfdir}/buran
%config(noreplace) %{_sysconfdir}/buran/buran.yaml
%dir /usr/lib/buran
%dir /usr/lib/buran/modules
%{_unitdir}/buran.service
%{_sysusersdir}/buran.conf

%files php
/usr/lib/buran/modules/buran-php%{php_suffix}

%changelog
* Sun Jul 12 2026 Buran Project <buran@example.invalid> - 0.1.0-1
- Reference packaging entry; the release workflow rewrites the version.
