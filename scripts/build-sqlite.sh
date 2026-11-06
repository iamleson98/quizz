#!/usr/bin/env bash
# Build a static SQLite library from the sqlite/sqlite repository's
# `begin-concurrent` branch and install it into third_party/sqlite-build/
# so that sqlx (via libsqlite3-sys env linking) uses THIS build.
#
# Requirements: cc, make, git, tclsh (8.5+; needed to generate the amalgamation).
#
# Cross-compiling: the amalgamation is generated with the build machine's own tools, and only the
# final compile and archive use CC and AR, so a cross toolchain builds a library for another
# machine (CC=x86_64-linux-gnu-gcc AR=x86_64-linux-gnu-ar ./scripts/build-sqlite.sh). The service
# is then built for the same target; see "Cross-compiling" in the README.
#
# Environment overrides:
#   CC               C compiler for the library (default: cc), a command name without arguments
#   AR               archiver for the library (default: ar)
#   SQLITE_GIT_REF   git ref to build (default: the pinned begin-concurrent commit)
#   SQLITE_REPO      repository URL (default: https://github.com/sqlite/sqlite)
#   SQLITE_OUT       output directory (default: third_party/sqlite-build)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="${SQLITE_REPO:-https://github.com/sqlite/sqlite}"
# Pinned commit on the begin-concurrent branch (SQLite 3.55.0).
# Update deliberately; record the new hash in third_party/sqlite-build/BUILD_INFO.
REF_DEFAULT="71f1e0e7384acdd2f80814a52df5f68cc4b99f33"
REF="${SQLITE_GIT_REF:-$REF_DEFAULT}"
OUT="${SQLITE_OUT:-$ROOT/third_party/sqlite-build}"
SRC="$ROOT/third_party/sqlite"
CC="${CC:-cc}"
AR="${AR:-ar}"

echo "==> sqlite source: $REPO @ ${REF}"
echo "==> output:        $OUT"

command -v cc >/dev/null     || { echo "cc not found" >&2; exit 1; }
command -v "$CC" >/dev/null  || { echo "$CC not found" >&2; exit 1; }
command -v "$AR" >/dev/null  || { echo "$AR not found" >&2; exit 1; }
command -v make >/dev/null   || { echo "make not found" >&2; exit 1; }
command -v git >/dev/null    || { echo "git not found" >&2; exit 1; }
if ! command -v tclsh >/dev/null; then
    echo "tclsh not found - it is required to generate the SQLite amalgamation." >&2
    echo "Install tcl (e.g. apt-get install tcl) and re-run." >&2
    exit 1
fi

# ---------------------------------------------------------------- fetch source
if [ -d "$SRC/.git" ]; then
    echo "==> updating existing clone at $SRC"
    git -C "$SRC" fetch origin begin-concurrent --depth 1
    git -C "$SRC" checkout --force "$REF"
else
    echo "==> cloning (shallow)"
    mkdir -p "$(dirname "$SRC")"
    git clone --depth 1 --branch begin-concurrent "$REPO" "$SRC"
    if [ "$REF" != "begin-concurrent" ]; then
        git -C "$SRC" fetch origin "$REF" --depth 1
        git -C "$SRC" checkout --force "$REF"
    fi
fi
COMMIT="$(git -C "$SRC" rev-parse HEAD)"

# ------------------------------------------------------- generate amalgamation
# An out-of-tree build dir keeps the source tree clean.
BUILD="$SRC/.build"
mkdir -p "$BUILD"
cd "$BUILD"
# The generators (lemon, mksourceid, ...) run on this machine: configure and make see no CC or AR,
# so a cross compiler set for the library below does not reach them.
if [ ! -f Makefile ]; then
    env -u CC -u AR ../configure
fi
env -u CC -u AR make sqlite3.c
SQLITE3_C="$BUILD/sqlite3.c"
[ -f "$SQLITE3_C" ] || SQLITE3_C="$SRC/sqlite3.c"
[ -f "$SQLITE3_C" ] || { echo "amalgamation sqlite3.c not found" >&2; exit 1; }
HDR="$BUILD/sqlite3.h"
[ -f "$HDR" ] || HDR="$SRC/sqlite3.h"
[ -f "$HDR" ] || { echo "sqlite3.h not generated" >&2; exit 1; }
EXT_H="$BUILD/sqlite3ext.h"
[ -f "$EXT_H" ] || EXT_H="$SRC/sqlite3ext.h"

VERSION="$(grep -m1 '^#define SQLITE_VERSION ' "$HDR" | awk '{print $3}')"
echo "==> building libsqlite3.a for SQLite ${VERSION} (${COMMIT:0:12})"

# ---------------------------------------------------------------- compile flags
# Mirrors libsqlite3-sys' bundled build flags (sqlx compatibility: API_ARMOR,
# COLUMN_METADATA, FTS*, RTREE, STAT4, LOAD_EXTENSION, SOUNDEX, UNLOCK_NOTIFY,
# ...), plus performance-oriented additions (MEMSTATUS=0, WAL_SYNCHRONOUS=1,
# MATH_FUNCTIONS, GEOPOLY, DESERIALIZE) on top of -O3.
CFLAGS_SQLITE=(
    -O3 -fPIC -pthread
    -DSQLITE_CORE
    -DSQLITE_THREADSAFE=1
    -DSQLITE_DEFAULT_FOREIGN_KEYS=1
    -DSQLITE_DEFAULT_MEMSTATUS=0
    -DSQLITE_DEFAULT_WAL_SYNCHRONOUS=1
    -DSQLITE_ENABLE_API_ARMOR
    -DSQLITE_ENABLE_COLUMN_METADATA
    -DSQLITE_ENABLE_DBSTAT_VTAB
    -DSQLITE_ENABLE_DESERIALIZE
    -DSQLITE_ENABLE_FTS3
    -DSQLITE_ENABLE_FTS3_PARENTHESIS
    -DSQLITE_ENABLE_FTS5
    -DSQLITE_ENABLE_GEOPOLY
    -DSQLITE_ENABLE_LOAD_EXTENSION=1
    -DSQLITE_ENABLE_MATH_FUNCTIONS
    -DSQLITE_ENABLE_RTREE
    -DSQLITE_ENABLE_STAT4
    -DSQLITE_ENABLE_UNLOCK_NOTIFY
    -DSQLITE_SOUNDEX
    -DSQLITE_LIKE_DOESNT_MATCH_BLOBS
    -DSQLITE_USE_URI
    -DSQLITE_MAX_EXPR_DEPTH=10000
    -DHAVE_USLEEP=1
    -DHAVE_ISNAN
)

# ------------------------------------------------------------------- install
rm -rf "$OUT"
mkdir -p "$OUT/include" "$OUT/lib/pkgconfig"

"$CC" "${CFLAGS_SQLITE[@]}" -c "$SQLITE3_C" -o "$OUT/lib/sqlite3.o"
"$AR" rcs "$OUT/lib/libsqlite3.a" "$OUT/lib/sqlite3.o"
rm -f "$OUT/lib/sqlite3.o"

cp "$HDR" "$EXT_H" "$OUT/include/"

cat > "$OUT/lib/pkgconfig/sqlite3.pc" <<EOF
prefix=\${pcfiledir}/../..
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: SQLite (begin-concurrent)
Description: SQL database engine - custom build from github.com/sqlite/sqlite branch begin-concurrent @ ${COMMIT} (SQLite ${VERSION})
Version: ${VERSION}
Libs: -L\${libdir} -lsqlite3 -lpthread -ldl -lm
Cflags: -I\${includedir}
EOF

cat > "$OUT/BUILD_INFO" <<EOF
source:     ${REPO} (branch: begin-concurrent)
commit:     ${COMMIT}
version:    ${VERSION}
built:      $(date -u +%Y-%m-%dT%H:%M:%SZ)
builder:    $("$CC" --version | head -1)

configure:  ./configure && make sqlite3.c   (sqlite autosetup; requires tclsh)

compile flags:
  -O3 -fPIC -pthread
$(printf '  %s\n' "${CFLAGS_SQLITE[@]:3}")

outputs:
  include/sqlite3.h, include/sqlite3ext.h
  lib/libsqlite3.a              (static; linked into the service binary)
  lib/pkgconfig/sqlite3.pc      (found via SQLITE3_LIB_DIR by libsqlite3-sys)

notes:
  - The begin-concurrent branch allows multiple concurrent writers in WAL mode
    via optimistic page-level locking (SQLITE_BUSY_SNAPSHOT on conflict).
    See third_party/sqlite/doc/begin_concurrent.md
  - sqlx links this build through .cargo/config.toml env vars
    (LIBSQLITE3_SYS_USE_PKG_CONFIG=1, SQLITE3_LIB_DIR, SQLITE3_STATIC=1).
  - Rebuild with scripts/build-sqlite.sh. Override SQLITE_GIT_REF to pin
    another ref.
EOF

echo "==> done: $OUT"
ls -la "$OUT/lib" "$OUT/include"
