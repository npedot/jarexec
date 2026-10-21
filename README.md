# jarexec

**Ship Java applications without fat JARs.**

jarexec is a launcher written in Rust and distributed as a single binary. Instead of a fat JAR of hundreds of MB, you ship the application JAR (a few MB) and a **lockfile** listing its dependencies. On first run, jarexec downloads the dependencies from Maven Central and, if needed, a JRE. It verifies every file with SHA-256 and then launches the application.

```
myapp/
├── myapp            the jarexec binary, renamed (myapp.exe on Windows)
├── myapp.lock       dependencies + SHA-256 + main class + Java version
├── myapp-1.0.jar    the application code
└── lib/             (optional) non-public JARs shipped with the app
```

An application's dependencies change rarely, while releases are frequent. With jarexec an update replaces only the JAR and the lock, and dependencies already downloaded stay in the cache.

## Features

- **No Maven on the client.** Dependencies are resolved at build time by Maven (`dependency:list`). jarexec never interprets the pom.
- **Integrity.** The SHA-256 of every JAR, the application JAR included, is computed at build time and verified on the client.
- **Automatic Java.** jarexec uses a compatible installed Java (`JAVA_HOME`, `PATH`, common locations, sdkman, …). If none is found, it downloads a [Temurin](https://adoptium.net) JRE into the cache.
- **App-local cache.** Dependencies and the JRE go into `.jarexec/`, next to the binary. Uninstalling means deleting the folder. After an update, what older versions left behind is removed automatically.
- **Fast startup.** From the second run on, jarexec does not touch the network: it runs `java @argfile` directly and returns the JVM's exit code.
- **Cross-platform.** Linux, Windows and macOS, on x64 and arm64.

## Installation

Download the archive for your platform from the [Releases](../../releases) page:

| Platform | Archive |
|---|---|
| Linux x64 / arm64 | `jarexec-vX.Y.Z-linux-x64.tar.gz` / `…-linux-aarch64.tar.gz` |
| Windows x64 | `jarexec-vX.Y.Z-windows-x64.zip` |
| macOS Apple Silicon / Intel | `jarexec-vX.Y.Z-mac-aarch64.tar.gz` / `…-mac-x64.tar.gz` |

Each archive comes with a `.sha256` file.

Alternatively, build from source (Rust stable):

```bash
cargo install --git https://github.com/npedot/jarexec --locked
```

## Usage in three steps

### 1. At build time: list the dependencies

Add this execution of the `maven-dependency-plugin` to your `pom.xml`:

```xml
<plugin>
  <groupId>org.apache.maven.plugins</groupId>
  <artifactId>maven-dependency-plugin</artifactId>
  <version>3.8.1</version>
  <executions>
    <execution>
      <id>jarexec-deps</id>
      <phase>package</phase>
      <goals><goal>list</goal></goals>
      <configuration>
        <outputFile>${project.build.directory}/jarexec-deps.txt</outputFile>
        <includeScope>runtime</includeScope>
        <outputAbsoluteArtifactFilename>true</outputAbsoluteArtifactFilename>
        <sort>true</sort>
      </configuration>
    </execution>
  </executions>
</plugin>
```

The application JAR must be **thin**, that is without dependencies inside, and must have a `Main-Class` in its manifest (`maven-jar-plugin`). A complete example is in [`examples/hello`](examples/hello).

### 2. At build time: generate the lock

```bash
mvn package
jarexec lock --deps target/jarexec-deps.txt --jar target/myapp-1.0.jar
# → target/myapp.lock
```

| Option | Default | Purpose |
|---|---|---|
| `--java N` | inferred from the main class bytecode | minimum Java version |
| `--java-max N` | — | maximum accepted version for a system Java |
| `--java-source auto\|system\|download` | `auto` | `system`: installed Java only; `download`: always use its own JRE |
| `--jvm-arg=ARG` (repeatable) | — | JVM options, e.g. `--jvm-arg=-Xmx2g` (use the `=` form) |
| `--bundle PATTERN` (repeatable) | — | ship non-public JARs in `lib/`: `com.example.*`, `group:artifact-*` |
| `--repo URL` (repeatable) | Maven Central | additional public repositories |
| `--local-repo DIR` | `~/.m2/repository` | where to look for JARs when the listed paths are unreachable |
| `--name NAME` | JAR file name without version | application and lock name |
| `-o FILE` | `<name>.lock` next to the JAR | lock path (`lib/` is created next to it) |

`jarexec lock` also scans the classes of every dependency. If some dependency needs a newer Java than the inferred one, it prints a warning with the value to pass to `--java`.

### 3. Ship it

```bash
mkdir -p dist/myapp
cp jarexec dist/myapp/myapp              # the binary is named after the lock
cp target/myapp-1.0.jar target/myapp.lock dist/myapp/
cp -r target/lib dist/myapp/             # only if you used --bundle
tar czf myapp-linux-x64.tar.gz -C dist myapp
```

On the user's machine:

```bash
tar xzf myapp-linux-x64.tar.gz
./myapp/myapp --the-app-options
```

A renamed binary, such as `myapp`, acts purely as a launcher: it looks for `myapp.lock` next to itself and passes **all** arguments to the application. An archive holds a single binary, so you need one archive per platform.

## Updating an application

Ship the new JAR **together with the new lock** (and `lib/` if you use `--bundle`), never the JAR alone: the lock names the JAR file and records its SHA-256, so a JAR replaced on its own is rejected. Shipping the full archive again is fine, since dependencies and the JRE are not in it.

Instructions for users:

1. Close the application.
2. Extract the archive **into the existing folder**, overwriting files. Do not delete `.jarexec/`: it holds Java and the libraries already downloaded. Extracting elsewhere works too, but downloads everything again.
3. Optionally delete the JAR of the previous version.
4. Start the application. If the new version has new dependencies, the first run downloads them; later runs are offline again.

The first run after an update sees a new lock and redoes the full preparation. Right after it, jarexec removes from the cache whatever the current and the previous version do not use (the previous one is kept for a rollback). This happens only with the default `.jarexec/` cache: with `--cache-dir` or `JAREXEC_CACHE` the cache may be shared with other apps, so use `jarexec clean --unused` instead.

## Commands

```bash
jarexec run    [app.lock] [-- args]   # prepare and launch
jarexec fetch  [app.lock]             # fill the cache without launching
jarexec doctor [app.lock]             # platform, selected Java and cache status
jarexec clean  [--unused]             # empty the cache, or drop only what recent launches do not use
jarexec lock   --deps … --jar …       # generate the lock (at build time)
```

Common options: `--cache-dir DIR` and `--java-home DIR`. `run --refresh` redoes Java discovery and cache verification.

## Environment variables

| Variable | Effect |
|---|---|
| `JAREXEC_JAVA_HOME` | always use this Java |
| `JAREXEC_CACHE` | cache in another folder, useful for read-only installs |
| `JAREXEC_DEBUG=1` | show which Java and which command are used |
| `HTTPS_PROXY` / `NO_PROXY` | proxy for downloads |

## The lockfile

```toml
version = 1

[app]
name = "myapp"
jar = "myapp-1.0.jar"
sha256 = "…"
size = 48213
main = "com.example.myapp.Main"
jvm_args = ["-Xmx2g"]

[java]
version = "21"
source = "auto"
vendor = "temurin"
image = "jre"

[[repository]]
id = "central"
url = "https://repo.maven.apache.org/maven2"

[[dep]]
coord = "org.slf4j:slf4j-api:jar:2.0.16"
sha256 = "…"
size = 69435

[[dep]]
coord = "com.example:internal-lib:jar:1.0.0-SNAPSHOT"
sha256 = "…"
size = 12345
bundled = "lib/internal-lib-1.0.0-SNAPSHOT.jar"
```

The lock is regenerated on every build, so every setting belongs in `jarexec lock` options, not in hand edits to the file.

## Notes

- **Platform-specific native libraries.** Some poms pick native JARs based on the build OS (classifiers). In that case you need one lock per platform. You can generate them all on one machine by forcing the classifier with `-D…` on `mvn dependency:list@jarexec-deps`.
- **Non-public libraries.** Internal modules, SNAPSHOTs and JARs installed by hand into `.m2` cannot be downloaded, so ship them with `--bundle`.
- **Building on Windows and WSL.** `jarexec lock` translates `C:\…` paths to `/mnt/c/…` and also searches the local repository. For the opposite case, use `--local-repo`.
- **Repositories.** `<repositories>` in the pom are not read: pass them with `--repo`.

## Current limitations

- Private repositories with authentication (`settings.xml`, Nexus, Artifactory) are not supported: use `--bundle`.
- Classpath only, no module path.
- `jarexec package` is not implemented yet: build the distribution by hand.
- `clean --unused` keeps only what is referenced by launches: dependencies brought in by `jarexec fetch` alone, with no launch afterwards, are removed.
- The Windows and macOS binaries are built and tested in CI, but have seen little use on real machines.

## Use as a library

The crate is also a library, for tools that manage several jarexec applications, such as a suite installer that fills one shared cache:

```toml
[dependencies]
jarexec = { git = "https://github.com/npedot/jarexec", tag = "v0.4.0" }
```

```rust
use jarexec::{clean, fetch, java, lockfile::Lockfile, paths};

let lock = Lockfile::load(path)?;
let cache = paths::cache_dir(Some(shared_cache))?;
let guard = clean::cache_lock(&cache)?;
let _in_use = guard.read()?;
java::JavaResolver::new(&cache, None)?.resolve(&java::Requirement::from_lock(&lock.java)?)?;
fetch::Fetcher::new(&cache, &lock.repositories)?.fetch_all(&lock.remote_deps(), fetch::DEFAULT_THREADS)?;
```

Downloads draw a progress bar on the terminal. A program with its own interface can
receive the progress instead, for everything started inside a closure on that thread:

```rust
use jarexec::http::{ProgressSink, report_progress_to};

struct Bar;
impl ProgressSink for Bar {
    fn start(&self, label: &str, total: u64) { /* a transfer of `total` bytes starts */ }
    fn inc(&self, bytes: u64) { /* called from several download threads */ }
    fn finish(&self) {}
}

report_progress_to(std::sync::Arc::new(Bar), || jarexec::launch::prepare(&lock, &cache, None, false))?;
```

The library API follows the binary's version and may change between minor releases until 1.0.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

To publish a release: bump `version` in `Cargo.toml`, then `git tag vX.Y.Z && git push origin vX.Y.Z`. CI builds archives for every platform and creates the GitHub Release.

## License

[MIT](LICENSE)
