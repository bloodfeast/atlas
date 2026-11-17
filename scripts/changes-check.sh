#!/bin/sh
# The dev-pr changelog-fragment check (changes/README.md).
#
#   sh scripts/changes-check.sh PR BUILD ROOT < LISTING
#
# PR is the pull request's number; BUILD is `true` when it touches a path the
# build reads (dev-pr's `changes` job decides); ROOT is a checkout holding its
# changes/ folder; LISTING is its files as `status<TAB>path` lines (the
# pulls/N/files API).
#
# Fails a build-path pull request that adds neither changes/PR.md (or
# changes/PR-slug.md, or an imported outside pull request's
# changes/public-N.md) nor changes/PR.none, unless it removes fragments and
# writes CHANGELOG.md and Cargo.toml (the `cargo xtask version` commit);
# fails one that adds both; and fails any file
# it adds or changes in changes/ that is not a well-formed fragment, a marker
# or the README. `cargo xtask version` parses fragments by the same rules
# (xtask/src/version.rs, whose tests hold the two to the same verdicts).
# Builds nothing.
set -eu
pr=$1
build=$2
root=$3
tab=$(printf '\t')
bom=$(printf '\357\273\277')
listing=$(cat)
if ! command -v iconv >/dev/null 2>&1; then
    echo "::error::iconv is not installed: the check needs it to hold fragments to UTF-8"
    exit 1
fi

# The fragment format: prints why a fragment is malformed and fails, or
# prints nothing. UTF-8 without a byte order mark first, as the parser. The
# second argument is 1 for an imported fragment (changes/public-N.md), which
# alone takes, and must have, `author: <GitHub login>`.
check_fragment() {
    if ! iconv -f UTF-8 -t UTF-8 "$1" >/dev/null 2>&1; then
        echo "not UTF-8 text: save it as UTF-8"
        return 1
    fi
    if [ "$(dd if="$1" bs=3 count=1 2>/dev/null)" = "$bom" ]; then
        echo "starts with a byte order mark (BOM): save it as UTF-8 without one"
        return 1
    fi
    # A C0 control character other than a tab or a line ending (a CR only
    # directly before an LF; stray_control in xtask/src/release.rs): GitHub
    # reads a lone CR as a line break. Read as hex bytes, one per line, since
    # some awk and sed builds drop a CR before an LF themselves.
    if od -An -v -tx1 "$1" | tr -s ' \n' '\n\n' | awk '
        NF == 0 { next }
        p == "0d" && $1 != "0a" { bad = 1 }
        $1 < "20" && $1 != "09" && $1 != "0a" && $1 != "0d" { bad = 1 }
        { p = $1 }
        END { exit !(bad || p == "0d") }
    '; then
        echo "a control character other than a tab or a line ending, which GitHub can read as a line break or hide text with: remove it"
        return 1
    fi
    awk -v public="$2" '
        # A fence line: up to three spaces, then three or more backticks or
        # tildes (fence_run in xtask/src/release.rs). Sets fc, fn_, fk (its
        # indent) and frest.
        function fence_run(s,    ind, n) {
            if (!match(s, /^ ? ? ?(```|~~~)/)) return 0
            ind = RLENGTH - 3
            fc = substr(s, ind + 1, 1)
            n = 0
            while (substr(s, ind + 1 + n, 1) == fc) n++
            fn_ = n
            fk = ind
            frest = substr(s, ind + 1 + n)
            return 1
        }
        # A line indent in columns: a space is one, a tab moves to the next
        # multiple of four (indent in xtask/src/release.rs).
        function indent(s,    i, c, col) {
            col = 0
            for (i = 1; i <= length(s); i++) {
                c = substr(s, i, 1)
                if (c == " ") col++
                else if (c == "\t") col += 4 - col % 4
                else break
            }
            return col
        }
        # A heading GitHub renders that does not start at the first column:
        # one to six #, then a space, a tab or the end, after spaces, tabs,
        # or list or quote markers (nested_heading in xtask/src/release.rs).
        function nested_heading(s,    nested, t) {
            nested = 0
            while (1) {
                t = s
                sub(/^[ \t]+/, "", t)
                if (t != s) nested = 1
                s = t
                if (s ~ /^>/) { s = substr(s, 2); nested = 1; continue }
                if (match(s, /^([-*+]|[0-9]+[.)])[ \t]/) || match(s, /^([-*+]|[0-9]+[.)])$/)) {
                    s = substr(s, RLENGTH + 1); nested = 1; continue
                }
                break
            }
            return nested && (s ~ /^(#|##|###|####|#####|######)[ \t]/ || s ~ /^(#|##|###|####|#####|######)$/)
        }
        { sub(/\r$/, "") }
        NR == 1 {
            if ($0 != "---") { print "the front matter must open with ---"; bad = 1; exit }
            next
        }
        !closed && $0 == "---" { closed = 1; next }
        !closed {
            if ($0 ~ /^[ \t]*$/) next
            i = index($0, ":")
            if (i == 0) { print "a front matter line is not key: value: " $0; bad = 1; exit }
            k = substr($0, 1, i - 1); v = substr($0, i + 1)
            gsub(/^[ \t]+|[ \t]+$/, "", k); gsub(/^[ \t]+|[ \t]+$/, "", v)
            if (k in seen) { print "duplicate front matter key " k; bad = 1; exit }
            seen[k] = 1
            if (k == "bump") {
                if (v !~ /^(patch|feature|breaking|none)$/) { print "bump must be patch, feature, breaking or none, not " v; bad = 1; exit }
                bump = v
            } else if (k == "section") {
                if (v !~ /^(highlight|breaking|upgrade|feature|fix|security|known-limit)$/) {
                    print "section must be highlight, breaking, upgrade, feature, fix, security or known-limit, not " v; bad = 1; exit
                }
                section = v
            } else if (k == "author") {
                if (public != 1) { print "author is only for an imported fragment, changes/public-<n>.md"; bad = 1; exit }
                if (length(v) > 39 || v !~ /^[A-Za-z0-9]+(-[A-Za-z0-9]+)*$/) {
                    print "author must be a GitHub login (letters, digits and single hyphens, at most 39), not " v; bad = 1; exit
                }
                author = v
            } else { print "unknown front matter key " k " (only bump and section)"; bad = 1; exit }
            next
        }
        {
            fenced = 0
            if (fence) {
                fenced = 1
                # A line indented less than the opening fence ends the block
                # on GitHub, as it ends the list item the block sits in.
                if ($0 !~ /^[ \t]*$/ && indent($0) < fence_k) {
                    print "a line inside a fenced block is indented less than the fence that opened it, which ends the block on GitHub: indent every line of the block, its closing fence included, at least as far as its opening fence"; bad = 1; exit
                }
                if (fence_run($0) && fc == fence_c && fn_ >= fence_n && frest ~ /^[ \t]*$/) fence = 0
            } else if (fence_run($0) && !(fc == "`" && index(frest, "`"))) {
                fence = 1; fenced = 1; fence_c = fc; fence_n = fn_; fence_k = fk
            } else if ($0 ~ /^#/) { print "a heading in the body: the fold writes the headings"; bad = 1; exit
            } else if (nested_heading($0)) {
                print "a heading after an indent or a list marker, which GitHub renders as a heading: the fold writes the headings"; bad = 1; exit
            } else if ($0 ~ /^[ \t]*(=+|-+)[ \t]*$/) {
                print "a line of only = or -, which GitHub renders as a heading underline under the line above: reword it"; bad = 1; exit
            }
            if (!started) {
                if ($0 ~ /^[ \t]*$/) next
                if ($0 !~ /^- /) { print "the body must be a list: its first line starts with \"- \""; bad = 1; exit }
                started = 1
            }
            # GitHub reads such a line as HTML, which can hide everything
            # after it (an open comment, <details>): comments are refused too.
            if (!fenced && $0 ~ /^[ \t]*</) {
                print "a line starts with < outside a fenced block, which GitHub reads as HTML that can hide the rest of the notes: reword it, or put it in a fenced block"; bad = 1; exit
            }
        }
        END {
            if (bad) exit 1
            if (NR == 0) { print "the front matter must open with ---"; exit 1 }
            if (!closed) { print "the front matter has no closing ---"; exit 1 }
            if (public == 1 && author == "") { print "no author in the front matter: an imported fragment names its pull request'"'"'s author"; exit 1 }
            # An upgrade note may leave its bump out; no other section may.
            if (bump == "" && section != "upgrade") { print "no bump in the front matter"; exit 1 }
            if (section == "") { print "no section in the front matter"; exit 1 }
            # bump: none on a breaking section falls to the none rule below.
            if (section == "breaking" && bump != "breaking" && bump != "none") { print "section: breaking needs bump: breaking"; exit 1 }
            if (!started) { print "the body is empty"; exit 1 }
            if (fence) { print "an unclosed fenced block: close it with a fence of the same character, at least as long"; exit 1 }
            # The contradictions, last, so every other fault keeps its own
            # reason: an upgrade note has no bump of its own, none is only for
            # an upgrade note, and a fix or security fragment is a patch.
            if (section == "upgrade" && bump != "" && bump != "none") { print "section: upgrade takes no bump of its own: write bump: none or leave it out"; exit 1 }
            if (section != "upgrade" && bump == "none") { print "bump: none is only for section: upgrade"; exit 1 }
            if (section == "fix" && bump != "patch") { print "section: fix needs bump: patch"; exit 1 }
            if (section == "security" && bump != "patch") { print "section: security needs bump: patch"; exit 1 }
        }
    ' "$1"
}

fail=0
own=0
none=0
present=$(printf '%s\n' "$listing" | grep -v "^removed$tab" | cut -f2 | grep '^changes/' || true)
while IFS= read -r f; do
    [ -n "$f" ] || continue
    name=${f#changes/}
    # The path as a workflow command carries it: no %, CR or LF, which could
    # end the command or start another, and no : or , (made _), which could
    # end the file property or add another.
    safe=$(printf '%s' "$f" | tr -d '%\r\n' | tr ':,' '__')
    if [ "$name" = README.md ]; then
        continue
    fi
    if printf '%s\n' "$name" | grep -Eq '^[1-9][0-9]*\.none$'; then
        [ "$name" != "$pr.none" ] || none=1
        continue
    fi
    if ! printf '%s\n' "$name" | grep -Eq '^(public-)?[1-9][0-9]*(-[a-z0-9]+)*\.md$'; then
        echo "::error file=$safe::$safe: not a fragment name (changes/<pr>.md, changes/<pr>-<slug>.md, changes/<pr>.none or an imported changes/public-<n>.md)"
        fail=1
        continue
    fi
    # An import's pull request carries the outside pull request's fragment
    # as its own (`cargo xtask release import`).
    public=0
    case $name in
        "$pr.md" | "$pr"-*) own=1 ;;
        public-*) own=1; public=1 ;;
    esac
    if ! msg=$(check_fragment "$root/$f" "$public"); then
        echo "::error file=$safe::$safe: $msg"
        fail=1
    fi
done <<EOF
$present
EOF

# A fold removes other pull requests' fragments; removing its own does not count.
folded=$(printf '%s\n' "$listing" | grep -E "^removed${tab}changes/((public-)?[1-9][0-9]*(-[a-z0-9]+)*\.md|[1-9][0-9]*(-[a-z0-9]+)*\.none)$" \
    | grep -vE "^removed${tab}changes/$pr(-[a-z0-9]+)*\.(md|none)$" || true)
# A fold also writes CHANGELOG.md and Cargo.toml: deleting another pull
# request's fragment is not a way around adding one's own.
if [ -n "$folded" ]; then
    if ! printf '%s\n' "$listing" | grep -qx "modified${tab}CHANGELOG.md" \
        || ! printf '%s\n' "$listing" | grep -qx "modified${tab}Cargo.toml"; then
        folded=
    fi
fi
if [ "$own" = 1 ] && [ "$none" = 1 ]; then
    echo "::error::changes/$pr.none marks a pull request with no user-facing change, but this one also adds a fragment: keep one"
    fail=1
fi
if [ "$build" = true ] && [ "$own" = 0 ] && [ "$none" = 0 ] && [ -z "$folded" ]; then
    echo "::error::this pull request touches a build path but adds no changelog fragment: add changes/$pr.md, or an empty changes/$pr.none for a docs, CI or test-only change (changes/README.md)"
    fail=1
fi
if [ "$fail" = 0 ]; then
    echo "changelog fragment check: passed"
fi
exit "$fail"
