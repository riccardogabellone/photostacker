# The cmake toolchain file of the libheif build: libde265 is its only codec,
# whatever libraries the build host has.
#
# libheif-sys builds libheif from its vendored source with cmake and turns
# every codec option on (`-DWITH_<codec>=ON`). libheif's CMakeLists.txt then
# looks for each codec library with a `find_package` that is not REQUIRED, and
# links every one it finds: x265 and x264 (GPL, which this MIT app must never
# link), but also libjpeg, AOM, dav1d and the rest. What the app links must not
# depend on what happens to be installed where it is built, so every package
# libheif looks for but LIBDE265 and two helpers (below) is hidden here: cmake
# skips a `find_package(<name>)`
# that is not REQUIRED when `CMAKE_DISABLE_FIND_PACKAGE_<name>` is true. The
# name must be spelled exactly as the `find_package` call spells it.
#
# `.cargo/config.toml` points `CMAKE_TOOLCHAIN_FILE` here; the `cmake` crate
# passes it on to cmake. The test `libde265_is_the_only_codec` in
# `crates/core/src/decode/heic.rs` checks what libheif ended up with.
#
# When libheif is updated, list the calls in the new libheif-sys's folder:
#
#     grep -rn --include=CMakeLists.txt find_package vendor/libheif
#
# Every name there belongs below except LIBDE265 and the two helpers: `Threads`
# (the threads library, REQUIRED by some codecs, and no codec itself) and
# `PkgConfig` (libde265 is found through it; libheif-sys turns off the
# gdk-pixbuf plugin that asks for it). The list below is that of libheif
# 1.23.1. After a change, run `cargo clean -p libheif-sys` (cargo does not
# build libheif again when only this file changes), then that test.

foreach(package IN ITEMS
    # HEVC, VVC and AVC encoders and decoders
    X265
    kvazaar
    UVG266
    vvdec
    vvenc
    X264
    OpenH264
    # AV1 encoders and decoders
    DAV1D
    AOM
    SvtEnc
    RAV1E
    # JPEG and JPEG 2000
    JPEG
    OpenJPEG
    OPENJPH
    # The FFmpeg HEVC decoder
    FFMPEG
    # Colour conversion, the compression of uncompressed images and headers,
    # and the API documentation
    libsharpyuv
    ZLIB
    Brotli
    Doxygen
)
  set(CMAKE_DISABLE_FIND_PACKAGE_${package} TRUE)
endforeach()
