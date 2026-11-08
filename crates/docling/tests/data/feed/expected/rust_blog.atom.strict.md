# Rust Blog

Empowering everyone to build reliable and efficient software.

## Demoting i686 Windows targets to std-only

Updated on: 2026-10-02T00:00:00+00:00

With Rust 1.100.0, the following changes to 32-bit Windows targets will happen:

- `i686-pc-windows-msvc` Tier 1 *with* host tools target will be demoted to Tier 1 *without* host tools.
- `i686-pc-windows-gnu` Tier 2 *with* host tools target will be demoted to Tier 2 *without* host tools.

Builds of the standard library will continue to be distributed, but host tools such as the compiler will be no longer available. `i686-pc-windows-msvc` as a Tier 1 target still undergoes CI testing.

To build 32-bit Windows binaries, cross-compiling from a still-supported host toolchain (such as a 64-bit Windows ones) will be required from now on.

## Background

Desktop and Server 32-bit only x86 CPUs are no longer sold for over 15 years, and general 32-bit Windows support has ended in October 2025. This means that the development platforms these targets are meant for hardly exist these days, and even if they do exist they typically aren't capable enough for development.

Even on the modern x86_64 hardware, building i686 Windows toolchains has proven to be problematic. We have encountered compiler binaries crashing when built with the i686 MSVC target, and the GNU C++ toolchain failing with OOMs during LLVM build.

Considering all these things, cross-compiling these targets from a better supported one is what we have found to be the best solution forward. As part of that, we stopped producing host tools for these targets. For the time being, the prebuilt standard library is still available, and in case of `i686-pc-windows-msvc` still tested on CI.

## What Changes?

After Rust 1.100, it will no longer be possible to install toolchains on 32-bit Windows hosts. We recommend cross-compiling from a still-supported host (such as a 64-bit Windows toolchain) instead. Other 32-bit platforms are not impacted by this change.

For more details about these demotions, see [RFC 3999](https://github.com/rust-lang/rfcs/blob/51783df9a76c355de7ceebeae101cba47f8ca463/text/3999-std-only-i686-msvc.md) for `i686-pc-windows-msvc` demotion, and [MCP 1020](https://github.com/rust-lang/compiler-team/issues/1020) for `i686-pc-windows-gnu` demotion.

## Announcing Rust 1.99.0

Updated on: 2026-10-01T00:00:00+00:00

The Rust team is happy to announce a new version of Rust, 1.99.0. Rust is a programming language empowering everyone to build reliable and efficient software.

If you have a previous version of Rust installed via `rustup`, you can get 1.99.0 with:

```
$ rustup update stable
```

If you don't have it already, you can get [`rustup`](https://www.rust-lang.org/install.html) from the appropriate page on our website, and check out the [detailed release notes for 1.99.0](https://doc.rust-lang.org/stable/releases.html#version-1990-2026-10-01).

If you'd like to help us out by testing future releases, you might consider updating locally to use the beta channel (`rustup default beta`) or the nightly channel (`rustup default nightly`). Please [report](https://github.com/rust-lang/rust/issues/new/choose) any bugs you might come across!

## What's in 1.99.0 stable

### extern "C" variadics

Rust 1.99.0 stabilizes defining C-ABI variadic functions with "C" and "C-unwind" ABIs. Variadic functions defined this way use a variable argument list (`...`) and accept an arbitrary number of arguments. Rust could already call externally-defined variadic functions (e.g., `libc::printf`). With Rust 1.99, these functions can now be written in Rust itself:

```
/// SAFETY: must be called with (at least) 2 i32 arguments.
unsafe extern "C" fn sum(mut args: ...) -> i32 {
    // SAFETY: guaranteed by the caller.
    let a = unsafe { args.next_arg::<i32>() };
    let b = unsafe { args.next_arg::<i32>() };
    a + b
}

fn foo() -> i32 {
    unsafe { sum(0i32, 2i32) }
}
```

The type of `...` is [`VaList`](https://doc.rust-lang.org/std/ffi/struct.VaList.html), which is ABI-compatible with the C `va_list` type across targets. What types can be read from a `VaList` is guarded by the [`VaArgSafe`](https://doc.rust-lang.org/std/ffi/trait.VaArgSafe.html) trait.

For more details on c-variadic functions, see the [Reference](https://doc.rust-lang.org/reference/items/functions.html#c-variadic-functions). This release also stabilizes support for defining naked variadic functions with non-"C" ABIs, which must be written via inline assembly.

### Layout information from raw pointers

This release settles the safety requirements for retrieving the size and alignment on raw pointers to both `Sized` (trivially safe, already possible on stable) and non-`Sized` types.

This is done by stabilizing three functions:

- [`Layout::for_value_raw`](https://doc.rust-lang.org/stable/core/alloc/struct.Layout.html#method.for_value_raw)
- [`mem::size_of_val_raw`](https://doc.rust-lang.org/stable/core/mem/fn.size_of_val_raw.html)
- [`mem::align_of_val_raw`](https://doc.rust-lang.org/stable/core/mem/fn.align_of_val_raw.html)

### `Recommend against round-trip unleaking after Box::leak`

While there are no changes to the language semantics in Rust 1.99, we have updated the documentation on [`Box::leak`](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.leak) to recommend against patterns that later deallocate that memory. This was done because such code was found to have problematic interactions with current and future potential compiler optimizations, and is especially problematic with the upcoming stabilization of custom allocators. Instead, [`Box::into_non_null`](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.into_non_null) or [`Box::into_raw`](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.into_raw) should be preferred.

This guidance also applies to other `leak` functions in the standard library.

### Stabilized APIs

- [IntoIterator for Box&lt;[T; N]&gt;](https://doc.rust-lang.org/stable/std/iter/trait.IntoIterator.html#impl-IntoIterator-for-Box%3C%5BT;+N%5D,+A%3E)
- [IntoIterator for &amp;Box&lt;[T; N]&gt;](https://doc.rust-lang.org/stable/std/iter/trait.IntoIterator.html#impl-IntoIterator-for-%26Box%3C%5BT;+N%5D,+A%3E)
- [IntoIterator for &amp;mut Box&lt;[T; N]&gt;](https://doc.rust-lang.org/stable/std/iter/trait.IntoIterator.html#impl-IntoIterator-for-%26mut+Box%3C%5BT;+N%5D,+A%3E)
- [`VecDeque::retain_back`](https://doc.rust-lang.org/stable/std/collections/struct.VecDeque.html#method.retain_back)
- [`core::ffi::VaList`](https://doc.rust-lang.org/stable/core/ffi/struct.VaList.html)
- [`Box::into_non_null`](https://doc.rust-lang.org/stable/std/boxed/struct.Box.html#method.into_non_null)
- [`Box::from_non_null`](https://doc.rust-lang.org/stable/std/boxed/struct.Box.html#method.from_non_null)
- [`Vec::into_parts`](https://doc.rust-lang.org/stable/std/vec/struct.Vec.html#method.into_parts)
- [`Vec::from_parts`](https://doc.rust-lang.org/stable/std/vec/struct.Vec.html#method.from_parts)
- [`core::mem::size_of_val_raw`](https://doc.rust-lang.org/stable/core/mem/fn.size_of_val_raw.html)
- [`core::mem::align_of_val_raw`](https://doc.rust-lang.org/stable/core/mem/fn.align_of_val_raw.html)
- [`core::alloc::Layout::for_value_raw`](https://doc.rust-lang.org/stable/core/alloc/struct.Layout.html#method.for_value_raw)
- [`String::from_utf8_lossy_owned`](https://doc.rust-lang.org/stable/std/string/struct.String.html#method.from_utf8_lossy_owned)
- [`string::FromUtf8Error::into_utf8_lossy`](https://doc.rust-lang.org/stable/std/string/struct.FromUtf8Error.html#method.into_utf8_lossy)
- [`FusedIterator for StepBy<I>`](https://doc.rust-lang.org/stable/std/iter/struct.StepBy.html#impl-FusedIterator-for-StepBy%3CI%3E)
- [`std::fs::set_times`](https://doc.rust-lang.org/stable/std/fs/fn.set_times.html)
- [`std::fs::set_times_nofollow`](https://doc.rust-lang.org/stable/std/fs/fn.set_times_nofollow.html)

### Other changes

Check out everything that changed in [Rust](https://github.com/rust-lang/rust/releases/tag/1.99.0), [Cargo](https://doc.rust-lang.org/nightly/cargo/CHANGELOG.html#cargo-199-2026-10-01), and [Clippy](https://github.com/rust-lang/rust-clippy/blob/master/CHANGELOG.md#rust-199).

## Contributors to 1.99.0

Many people came together to create Rust 1.99.0. We couldn't have done it without all of you. [Thanks!](https://thanks.rust-lang.org/rust/1.99.0/)

## Announcing a Maintainer in Residence: Scott Schafer for the Cargo team

Updated on: 2026-09-22T00:00:00+00:00

At the end of August, we [announced](https://blog.rust-lang.org/2026/08/26/announcing-our-first-maintainers-in-residence/) our first Maintainers in Residence, Rust Project contributors who are funded for their upstream contributions and [maintenance work](https://blog.rust-lang.org/inside-rust/2026/01/12/what-is-maintenance-anyway/) from the [Rust Foundation Maintainers Fund](https://github.com/sponsors/rustfoundation) (RFMF). Since then, the Rust Leadership Council has dedicated more funds from its Project Priorities budget to RFMF, and together with AWS also providing additional funds, this allowed us to open a new full-time Maintainer in Residence (MiR) position to support the Cargo team. We would like to thank the Rust Leadership Council, AWS, and also the Rust Foundation for providing us with this opportunity! If you would like to help us hire more maintainers to improve Rust, consider [donating](https://github.com/sponsors/rustfoundation) to RFMF.

This post explains why we chose to support the Cargo team specifically, and introduces [Scott Schafer](https://blog.rust-lang.org/2026/09/22/announcing-a-maintainer-in-residence-scott-schafer-for-the-cargo-team/#introducing-scott-schafer), the new Cargo Maintainer in Residence.

## Why Cargo?

The new MiR full-time position is dedicated to helping with the maintenance of [Cargo](https://github.com/rust-lang/cargo), our build system and package manager. The Cargo project is deeply involved in many new Rust features, improvements, and [Project Goals](https://goals.rust-lang.org/2026/goals.html#cargo-team). Combined with its cross-cutting nature, where it has to support many different use-cases and integrate with several other tools, it takes a lot of work just to keep up with its maintenance needs, let alone support so many feature requests and proposed changes.

Because of that, the Cargo team has sometimes struggled with meeting its maintenance demands. You might remember that for several years, it actually held a [feature freeze](https://blog.rust-lang.org/inside-rust/2022/03/31/cargo-team-changes/), to reduce Cargo's internal tech debt, perform necessary refactorings, go through the issue and pull request backlog, and come up with scalable internal development and design processes, so that they could eventually go back to even thinking about adding new features.

Recently, some changes occurred within the team, which made it more difficult for them to meet their maintenance baseline. Some members of the team left, while others lost their dedicated funding for working on Cargo maintenance and had to scale down their involvement. The Funding team thus considered it very important to support this team, given that we had an opportunity to do so. And thus we decided to hire a full-time maintainer to work on Cargo for (at least) the next 12 months.

Even though we know that a single full-time maintainer will not completely solve the maintenance struggles of the Cargo team, we hope that it will improve the situation, and provide a bit of a relief for the team.

## Introducing Scott Schafer

A photo of Scott Schafer

<!-- image -->

We are very happy to welcome Scott Schafer ([@muscraft](https://github.com/Muscraft)) into the Maintainer in Residence role! Scott has joined the Cargo team [three years ago](https://blog.rust-lang.org/inside-rust/2023/04/06/cargo-new-members/), and apart from working on Cargo, he is also the lead of the Rust [Docker team](https://rust-lang.org/governance/teams/#team-docker), which prepares official Docker images for every Rust version.

Apart from working on general maintenance of Cargo, Scott has implemented Cargo's [Workspace inheritance](https://doc.rust-lang.org/cargo/reference/workspaces.html#the-package-table) feature, and has also spearheaded a complex multi-year effort to switch the rendering of diagnostics in the Rust compiler to use the [annotate-snippets](https://github.com/rust-lang/annotate-snippets-rs) crate. This effort has been completed in the Rust 1.93.0 release. Thanks to it, the same diagnostics interface can now be shared between the compiler and Cargo (and also other tools), which amongst other things unblocked further development of the Cargo linting system, which has now been stabilized and will ship in the Rust 1.100.0 release.

Everyone we talked about was very excited about Scott becoming a Cargo Maintainer in Residence, and we share that feeling. We wish Scott all the best in his new role, and we are very happy that we can support his maintenance work.

Here is what Scott thinks about it:

I am incredibly excited to work on Cargo full-time! There have been so many things that I wish I could've worked on over the years, that I will now be able to get to. I hope that my efforts will bring Cargo into a more maintainable state.

## Conclusion

We are incredibly happy that we keep getting more funds for the Rust Foundation Maintainers Fund, which allows us to support Rust Project contributors. The funding team will be working with the supported maintainers, and also the funders, to ensure that they are all happy with the arrangement, so that we can secure stable funding for Rust maintenance for years to come.

If you would like to help us support more Rust maintainers, consider [donating](https://github.com/sponsors/rustfoundation) to RFMF!

## GitHub Actions leaking secrets when Miri output is cached

Updated on: 2026-09-21T00:00:00+00:00

The Rust Security Response Team was notified that Miri stores all environment variables to `target/`, allowing secrets to persist in caches.

While not necessary a vulnerability in and of itself, when paired with GitHub Actions caching behavior, it is possible for this to expose secrets to PRs.

## Overview

GitHub Actions makes it possible to cache directories between runs. Typical setups allow CI runs on `main` (and other branches) to *write* to cache, and PRs can only *read* from cache (preventing cache poisoning). Rust projects tend to speed up CI by caching binaries built by `cargo install` and sometimes the contents of `target/`.

PR CI can be triggered by anyone who can open PRs on your repository. GitHub requires maintainer approval for the *first* PR, but future PRs will rerun CI on every push. Anyone who has previously landed a change can trigger a CI run extracting information from cached `target/` and then cover their tracks by pushing a second commit to the PR.

GitHub sometimes hides overwritten commits in its UI, making this kind of attack harder to detect. CI run logs and overwritten commits are also deleted after a few months.

When `cargo miri` is invoked, Miri needs to retain build-relevant environment variables between runs[[1]](https://blog.rust-lang.org/2026/09/21/github-actions-leaking-secrets-when-miri-output-is-cached/#fn-1). The current code to do so achieves this by storing [all environment variables to target/](https://github.com/rust-lang/miri/blob/165a9c3c96f0f4e6232278e62cb64648215bbc37/cargo-miri/src/util.rs#L40-L42). This, of course, persists when `target/` is cached.

If your environment contained secrets, these can now be accessed by PRs via the cache.

## Our fix

Our [short term fix](https://github.com/rust-lang/miri/pull/5337) for this is to make Miri only preserve `CARGO_*` environment variables (excepting `CARGO_*_TOKEN`) and `OUT_DIR`. In the longer term, Miri and cargo may figure out better ways to inform Miri of the relevant list of environment variables. Note that this patch may not be available on nightly yet.

We also performed an ecosystem scan of GitHub repositories and identified 1 repository with this issue and 7 repositories that do not appear to be vulnerable but should be cautious anyway. We have reached out to those maintainers.

## Am I affected?

It is likely that our scan was imperfect, so we recommend you check your own GitHub Actions setups if you run Miri.

You are vulnerable if:

- You run `cargo miri` in CI
- The step that runs `cargo miri` [has access to secrets](https://docs.github.com/en/actions/how-tos/write-workflows/choose-what-workflows-do/use-secrets#using-secrets-in-a-workflow) as an environment variable:
    - By being passed in to the step itself as an environment variable
    - By being set in `env` for the workflow
    - By being passed in to a previous step that persists it in the environment somehow
- The workflow being used caches the `target` directory, usually done via [`actions/cache`](https://github.com/actions/cache) or [`swatinem/rust-cache`](https://github.com/swatinem/rust-cache)
- The cache is accessible to PRs (common and often the intended use case)

Possible quick fixes include:

- Disabling cache for that job.
- Scoping secrets to steps in that job that do not call Miri.
- Temporarily disabling Miri.

Once done, [please clear the cache](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manage-caches#deleting-cache-entries). Consider rotating any secrets that might have leaked.

The Miri release in the upcoming nightly (2026-09-22) will no longer have this problem.

**Even if you do not run Miri,** ensure jobs that can write to public caches do not have access to secrets. Many tools do not have special handling for secrets, and assume the entire environment can be written to the filesystem.

## Threat model

We consider it bad practice to have a cache that can easily be tainted by secrets.

If caching `target/`, it is worth making sure that the inputs to processes that create `target/` (anything invoking `cargo`) do not have secrets available. It is generally rare for standard `cargo` build/test subcommands to need any secrets or tokens[[2]](https://blog.rust-lang.org/2026/09/21/github-actions-leaking-secrets-when-miri-output-is-cached/#fn-2), so this is mostly a matter of being careful about having secrets exposed as environment variables to the entire job.

Cargo/Miri/Rust does not guarantee that environment variables will be safe from being copied into `target/`. While we are treating this as a security issue and patching it out of an abundance of caution, this is not something you should rely on in general. Beyond official Rust tooling, it is possible for build scripts to be doing things that lead to the environment being stored in compilation artifacts.

## Acknowledgements

Thanks to [Predrag Gruevski](https://github.com/obi1kenobi) of OpenAI for reporting this issue to us. Furthermore, the ecosystem scan was performed using Codex access and credits donated by OpenAI, which we also thank them for.

Issue triage and remediation was performed by Manish Goregaokar, Ralf Jung, Ben Kimock, Weihang Lo, Jacob Finkelman, Walter Pearce, Josh Stone, and Mark Rousskov.

1. Miri is invoked multiple times by `cargo miri` for complicated reasons [↩](https://blog.rust-lang.org/2026/09/21/github-actions-leaking-secrets-when-miri-output-is-cached/#fr-1-1)
2. In theory it could come up with build scripts reading from the network [↩](https://blog.rust-lang.org/2026/09/21/github-actions-leaking-secrets-when-miri-output-is-cached/#fr-2-1)
