# Typechecking this crate on an ordinary Linux host

Before this, you could not `cargo check` arkkvm without a full device build environment, and
that environment is not obtainable from the published repositories. This documents what is
actually required, what is missing upstream, and how to assemble enough to typecheck.

## What the README asks for, and what is actually published

The README directs you to [arkkvm-system](https://github.com/arkkvm/arkkvm-system) for the
toolchain and sysroot. ⛔ **Neither is in that repository.** Verified against it directly:

| needed by `make_buildkit.sh` | published? | why not |
|---|---|---|
| `tools/linux/toolchain/arm-rockchip830-…` | ❌ | `tools/.gitignore` contains `linux/toolchain` |
| `media/out/include` (rockit headers) | ❌ | `media/.gitignore` contains `*/out`; `media/rockit/` holds only a Makefile |
| `media/out/lib` | ❌ | same |
| `sysdrv/tools/board/toolkits/{openssl,zlib}/out` | ❌ | same pattern |

`librockit.so` *does* ship prebuilt in the rootfs — so the library is published while the
headers to compile against it are not.

## Where the pieces actually are

The toolchain and the rockit headers are identical across RV1106 vendor SDKs, and two are
public: [luckfox-pico](https://github.com/LuckfoxTECH/luckfox-pico) and
[LubanCat RV06_03_Linux_SDK](https://github.com/LubanCat/RV06_03_Linux_SDK).

A sparse checkout avoids pulling the whole SDK — 233 MB instead of >1 GB:

```bash
git clone --filter=blob:none --no-checkout --depth 1 \
    https://github.com/LuckfoxTECH/luckfox-pico.git luckfox-sdk
cd luckfox-sdk
git sparse-checkout init --cone
git sparse-checkout set \
    tools/linux/toolchain/arm-rockchip830-linux-uclibcgnueabihf \
    media/rockit/rockit/mpi/sdk/include
git checkout HEAD
```

Assemble a `BUILDKIT_ROOT` in the layout `make_buildkit.sh` expects:

```bash
BK=$HOME/arkkvm-buildkit
SDK=$PWD                       # the luckfox-sdk checkout
mkdir -p "$BK"
cp -r "$SDK/tools/linux/toolchain/arm-rockchip830-linux-uclibcgnueabihf/." "$BK/"
cp -r "$SDK/media/rockit/rockit/mpi/sdk/include/." \
      "$BK/arm-rockchip830-linux-uclibcgnueabihf/sysroot/usr/include/"
```

Then:

```bash
mkdir -p frontend/dist && echo '<!doctype html>' > frontend/dist/index.html   # placeholder
BUILDKIT_ROOT=$BK cargo check -p arkkvm
```

## What this gets you, and what it does not

✅ **Typechecking the whole crate**, including bindgen over the real rockit headers. That is
enough to review a change, catch a type error, and run `cargo test`.

❌ **Not a firmware build.** That additionally needs the proprietary Rockchip `.so` set for
linking, prebuilt Opus, the real frontend `dist/`, and a **stage2 Rust compiler built from
Rust source tag 1.94.1** (`-Z build-std` against `armv7-unknown-linux-uclibceabihf`). Do not
flash anything produced this way.

⚠ The rockit headers come from a *different vendor's* SDK of the same SoC. They are the same
API, but if a binding ever looks wrong, suspect a version skew here first.

## Two changes were needed to make this work at all

Both are target-conditional and leave the device build byte-identical.

1. **`arkkvm/build.rs`** required `BUILDKIT_ROOT` unconditionally, so `cargo check` died before
   compiling a line of Rust. The `getauxval` shim exists because uClibc lacks that symbol;
   glibc already has it, so a host build does not need it. Now skipped with a warning when
   `BUILDKIT_ROOT` is unset and the target is not the uclibc ARM one.

2. **`arkkvm/src/hardware/hdmi/v4l2.rs`** cast the ioctl request to `u32`. libc spells that
   parameter differently per target — `u32` on 32-bit uclibc, `c_ulong` on 64-bit glibc — so
   the hard-coded cast was correct for the device and the last type error on a host. `as _`
   is right on both.
