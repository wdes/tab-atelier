# 05 — Build, versioning and networking configuration

**Upstreamable: no**, with one exception that should be revisited rather than offered.

## The changes

- **Versioning.** `app/build.gradle.kts` removes upstream's `app-versioning` plugin, which
  derives the version code and name from git tags. The fork sets the version from Gradle
  properties and its own CI counter (`scripts/build-apk.sh`, `android-apk.yml`). The plugin
  is still declared in `gradle/libs.versions.toml`; only its use in `app/build.gradle.kts` is
  removed, so the catalogue entry is dead and can be dropped in a sync.
- **OkHttp.** `gradle/libs.versions.toml` adds `okhttp` 5.5.0. The tab-atelier client uses it
  for HTTP and WebSocket. Upstream has no HTTP client in the app module, so this is a new
  dependency, not a change to one of theirs.
- **Cleartext traffic.** `AndroidManifest.xml` sets `android:usesCleartextTraffic="true"`.

## Cleartext traffic needs revisiting

Enabling cleartext lets the app use `http://` on any host. Tab-atelier needs it because a LAN
daemon is often plain HTTP, but the setting is application-wide, so it also permits cleartext to
hosts the user never configured. The narrower option is a `network_security_config` that
permits cleartext only for the LAN ranges, or only for hosts the user has marked local. Upstream
would reject the global form, so it must not be offered upstream in its current shape.

## What a sync must watch

`app/build.gradle.kts` changed 25 times upstream and `gradle/libs.versions.toml` 55 times since
the base, so these are the first files a sync touches. Re-apply the versioning removal and the
OkHttp entry on top of upstream's current catalogue, and check the Gradle wrapper and AGP
versions in the same pass.
