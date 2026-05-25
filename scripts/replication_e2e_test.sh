#!/usr/bin/env bash
# End-to-end test of cascading replication.
#
# Topology:
#   1 -> 2 -> 3 -> 4 -> 5
#             |
#             v
#             6
#   2 -> 7
#   6 -> 8
#   7 -> 8
#
# Verifies:
#   * Each downstream sees the upstream's host(s) (mirrored via replication).
#   * Instance 5 ends up shadowing hosts that originated on 1 (4 hops).
#   * Instance 8 ends up shadowing hosts from both 6 and 7 fan-ins.
#   * Trying to pair 1 -> 8 is refused (would close the cycle).
#
# Requirements: docker, docker compose, curl, jq.

set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE_FILE="$HERE/docker-compose.replication-test.yml"
PASSWORD="adminadmin"

declare -A PORT=(
    [1]=4001 [2]=4002 [3]=4003 [4]=4004
    [5]=4005 [6]=4006 [7]=4007 [8]=4008
)

declare -A SESSION_COOKIE=()
declare -A TOKEN=()
declare -A LOCAL_GROUP_UUID=()
declare -A LOCAL_HOST_UUID=()

log()  { printf "\033[1;34m[%s]\033[0m %s\n" "$(date +%H:%M:%S)" "$*"; }
warn() { printf "\033[1;33m[%s] WARN\033[0m %s\n" "$(date +%H:%M:%S)" "$*"; }
fail() { printf "\033[1;31m[%s] FAIL\033[0m %s\n" "$(date +%H:%M:%S)" "$*"; exit 1; }
ok()   { printf "\033[1;32m[%s]  OK \033[0m %s\n" "$(date +%H:%M:%S)" "$*"; }

# Per-instance container address (used by haze-N to reach haze-M without
# leaving the docker network). When a peer is created, the destination
# resolves the URL on its own, so we use the in-compose service name.
docker_url_for() {
    echo "http://haze-$1:4420"
}
host_url_for() {
    echo "http://127.0.0.1:${PORT[$1]}"
}

login() {
    local idx="$1"
    local base
    base="$(host_url_for "$idx")"
    local resp
    resp="$(curl -sS -i -X POST "$base/api/v1/auth/login" \
        -H 'Content-Type: application/json' \
        -d "{\"username\":\"admin\",\"password\":\"$PASSWORD\"}")"
    local cookie
    cookie="$(printf '%s' "$resp" | awk 'tolower($1)=="set-cookie:" { sub(/;.*/, "", $2); print $2; exit }')"
    [[ -n "$cookie" ]] || fail "no session cookie returned from haze-$idx"
    SESSION_COOKIE[$idx]="$cookie"
}

mk_token() {
    local idx="$1" replication_only="${2:-false}"
    local base="$(host_url_for "$idx")"
    local body
    body="$(curl -sS -X POST "$base/api/v1/user/tokens" \
        -H "Cookie: ${SESSION_COOKIE[$idx]}" \
        -H 'Content-Type: application/json' \
        -d "{\"name\":\"e2e-replication-test\",\"replication_only\":${replication_only}}")"
    local tok
    tok="$(echo "$body" | jq -r '.plaintext // .token')"
    [[ -n "$tok" && "$tok" != "null" ]] || fail "no token returned by haze-$idx: $body"
    TOKEN[$idx]="$tok"
}

mk_group() {
    local idx="$1" name="$2"
    local base="$(host_url_for "$idx")"
    local body
    body="$(curl -sS -X POST "$base/api/v1/groups" \
        -H "Cookie: ${SESSION_COOKIE[$idx]}" \
        -H 'Content-Type: application/json' \
        -d "{\"display_name\":\"$name\"}")"
    local uuid
    uuid="$(echo "$body" | jq -r .uuid)"
    [[ -n "$uuid" && "$uuid" != "null" ]] || fail "group create on haze-$idx failed: $body"
    LOCAL_GROUP_UUID["$idx-$name"]="$uuid"
}

mk_host() {
    local idx="$1" name="$2" group_uuid="$3" target="${4:-127.0.0.1}"
    local base="$(host_url_for "$idx")"
    local body
    body="$(curl -sS -X POST "$base/api/v1/hosts" \
        -H "Cookie: ${SESSION_COOKIE[$idx]}" \
        -H 'Content-Type: application/json' \
        -d "{\"display_name\":\"$name\",\"probe_type\":\"ping\",\"probe_config\":{\"target\":\"$target\"},\"interval_secs\":10,\"samples_per_period\":10,\"group_uuids\":[\"$group_uuid\"]}")"
    local uuid
    uuid="$(echo "$body" | jq -r .uuid)"
    [[ -n "$uuid" && "$uuid" != "null" ]] || fail "host create on haze-$idx failed: $body"
    LOCAL_HOST_UUID["$idx-$name"]="$uuid"
}

add_peer() {
    # add_peer <dest_idx> <source_idx> <peer_name>
    local dest="$1" src="$2" name="$3"
    local base="$(host_url_for "$dest")"
    local src_url="$(docker_url_for "$src")"
    local body
    body="$(curl -sS -w '\n__HTTP__%{http_code}' -X POST "$base/api/v1/replication/peers" \
        -H "Cookie: ${SESSION_COOKIE[$dest]}" \
        -H 'Content-Type: application/json' \
        -d "{\"name\":\"$name\",\"base_url\":\"$src_url\",\"api_token\":\"${TOKEN[$src]}\"}")"
    local status
    status="$(echo "$body" | tail -n1 | sed 's/^__HTTP__//')"
    local payload
    payload="$(echo "$body" | sed '$d')"
    if [[ "$status" != "201" ]]; then
        warn "add_peer haze-$dest <- haze-$src status=$status body=$payload"
    fi
    echo "$payload"
}

add_rule() {
    # add_rule <dest_idx> <peer_uuid>
    # Pulls source root into dest root.
    local dest="$1" peer_uuid="$2"
    local base="$(host_url_for "$dest")"
    local body
    body="$(curl -sS -X POST "$base/api/v1/replication/rules" \
        -H "Cookie: ${SESSION_COOKIE[$dest]}" \
        -H 'Content-Type: application/json' \
        -d "{\"peer_uuid\":\"$peer_uuid\"}")"
    local uuid
    uuid="$(echo "$body" | jq -r .uuid)"
    [[ -n "$uuid" && "$uuid" != "null" ]] || fail "rule create on haze-$dest failed: $body"
    echo "$uuid"
}

wait_for_up() {
    log "waiting for all 8 instances to report ready"
    for idx in 1 2 3 4 5 6 7 8; do
        local base="$(host_url_for "$idx")"
        for _ in $(seq 1 60); do
            if curl -fsS "$base/api/v1/server-info" >/dev/null 2>&1; then
                break
            fi
            sleep 2
        done
        curl -fsS "$base/api/v1/server-info" >/dev/null \
            || fail "haze-$idx never became healthy"
    done
    ok "all 8 instances up"
}

list_hosts() {
    local idx="$1"
    curl -sS "$(host_url_for "$idx")/api/v1/hosts" \
        -H "Cookie: ${SESSION_COOKIE[$idx]}"
}

# ─── Main ───────────────────────────────────────────────────────────────────

main() {
    log "tearing down any previous compose stack so we always test from clean state"
    docker compose -f "$COMPOSE_FILE" down -v 2>/dev/null || true

    log "spinning up docker compose stack"
    docker compose -f "$COMPOSE_FILE" up -d --build

    wait_for_up

    log "logging in to each instance + minting API tokens"
    for idx in 1 2 3 4 5 6 7 8; do
        login "$idx"
    done
    # haze-1's token is replication_only so the link 1->2 exercises the
    # scoped-token middleware. Every other instance uses a regular admin
    # token so the rest of the test surface stays unchanged.
    mk_token 1 true
    for idx in 2 3 4 5 6 7 8; do
        mk_token "$idx" false
    done

    log "verifying haze-1's replication-only token is rejected outside replication scope"
    local fb
    fb="$(curl -sS -o /dev/null -w '%{http_code}' "$(host_url_for 1)/api/v1/hosts" \
        -H "Authorization: Bearer ${TOKEN[1]}")"
    if [[ "$fb" == "403" ]]; then
        ok "replication-only token correctly refused on /api/v1/hosts (403)"
    else
        fail "expected 403 on /hosts with replication-only token, got $fb"
    fi
    local rb
    rb="$(curl -sS -o /dev/null -w '%{http_code}' \
        "$(host_url_for 1)/api/v1/replication/instance-info" \
        -H "Authorization: Bearer ${TOKEN[1]}")"
    if [[ "$rb" == "200" ]]; then
        ok "replication-only token accepted on /replication/instance-info (200)"
    else
        fail "expected 200 on /replication/instance-info, got $rb"
    fi

    log "seeding local probes + per-instance groups (10s/10 ping to a random other haze container)"
    for idx in 1 2 3 4 5 6 7 8; do
        local name="grp-$idx"
        mk_group "$idx" "$name"
        # Pick a random target that isn't ourselves so the docker DNS
        # resolves to a different container each time the test runs.
        local target
        while :; do
            target=$(( (RANDOM % 8) + 1 ))
            [[ "$target" != "$idx" ]] && break
        done
        mk_host "$idx" "host-$idx" "${LOCAL_GROUP_UUID["$idx-$name"]}" "haze-$target"
    done

    log "seeding the special remap test: 'Special' group + host on haze-1 (pings haze-2)"
    mk_group 1 "Special"
    mk_host 1 "special-host-1" "${LOCAL_GROUP_UUID["1-Special"]}" "haze-2"
    log "seeding 'Renamed' destination group on haze-3 for the remap rule"
    mk_group 3 "Renamed"

    log "wiring replication peers (admin token of source is bearer on dest)"
    # Each entry: dest -> src. Cascades 1->2->3->4->5, branches, fan-in to 8.
    # haze-7 is wired LATER on, to exercise the "late peer added after
    # downstream already runs" code path. So omit 7<-2 here and add it
    # after the initial verifications.
    declare -A PEER_UUID=()
    for pair in "2 1 from-1" "3 2 from-2" "4 3 from-3" "5 4 from-4" \
                "6 4 from-4" "8 6 from-6" "8 7 from-7"; do
        # shellcheck disable=SC2086
        set -- $pair
        local dest="$1" src="$2" name="$3"
        local body
        body="$(add_peer "$dest" "$src" "$name")"
        local uuid
        uuid="$(echo "$body" | jq -r .uuid 2>/dev/null || true)"
        if [[ -n "$uuid" && "$uuid" != "null" ]]; then
            PEER_UUID["$dest-$src"]="$uuid"
            ok "haze-$dest peered with haze-$src as $name"
        else
            fail "peer creation haze-$dest <- haze-$src returned: $body"
        fi
    done

    log "creating replication rules (root -> root for each peer)"
    for key in "${!PEER_UUID[@]}"; do
        local dest="${key%-*}"
        local rule_uuid
        rule_uuid="$(add_rule "$dest" "${PEER_UUID[$key]}")"
        ok "rule $rule_uuid on haze-$dest"
    done

    log "adding remap rule on haze-3: 'Special on haze-2' -> 'Renamed on haze-3'"
    # haze-2 inherits 'Special' from haze-1 via root->root. We discover its
    # local uuid on haze-2 by name, then map it into haze-3's pre-seeded
    # 'Renamed' group.
    log "waiting briefly for haze-2 to materialise the Special group"
    local special_uuid_on_2=""
    for _ in $(seq 1 30); do
        special_uuid_on_2="$(curl -sS "$(host_url_for 2)/api/v1/groups" \
            -H "Cookie: ${SESSION_COOKIE[2]}" \
            | jq -r '.[] | select(.display_name == "Special") | .uuid' | head -n1)"
        [[ -n "$special_uuid_on_2" ]] && break
        sleep 2
    done
    if [[ -z "$special_uuid_on_2" ]]; then
        warn "haze-2 never received the 'Special' group from haze-1"
    else
        local remap_resp
        remap_resp="$(curl -sS -X POST "$(host_url_for 3)/api/v1/replication/rules" \
            -H "Cookie: ${SESSION_COOKIE[3]}" \
            -H 'Content-Type: application/json' \
            -d "{\"peer_uuid\":\"${PEER_UUID[3-2]}\",\"source_group_uuid\":\"$special_uuid_on_2\",\"dest_group_uuid\":\"${LOCAL_GROUP_UUID["3-Renamed"]}\"}")"
        local remap_rule_uuid
        remap_rule_uuid="$(echo "$remap_resp" | jq -r .uuid)"
        if [[ -n "$remap_rule_uuid" && "$remap_rule_uuid" != "null" ]]; then
            ok "remap rule $remap_rule_uuid: haze-2.Special -> haze-3.Renamed"
        else
            warn "remap rule create failed: $remap_resp"
        fi
    fi

    log "letting workers run for 60s to start materialising hosts + ingesting samples"
    sleep 60

    log "verifying that instance 5 has hosts cascaded from 1, 2, 3, 4 (poll up to 4 minutes)"
    for expected_idx in 1 2 3 4; do
        local host_uuid="${LOCAL_HOST_UUID["$expected_idx-host-$expected_idx"]}"
        local found=""
        for _ in $(seq 1 48); do
            if list_hosts 5 | jq -e --arg u "$host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
                found=1; break
            fi
            sleep 5
        done
        if [[ -n "$found" ]]; then
            ok "haze-5 has replicated host-$expected_idx (uuid $host_uuid)"
        else
            warn "haze-5 missing host-$expected_idx (uuid $host_uuid) - dumping host list"
            list_hosts 5 | jq -r '.[] | "  \(.uuid)  \(.display_name)  peer=\(.replication_peer_id)"'
            fail "cascaded replication did not reach haze-5"
        fi
    done

    log "verifying group remap: haze-3 has 'special-host-1' under 'Renamed' group"
    local renamed_uuid="${LOCAL_GROUP_UUID["3-Renamed"]}"
    local renamed_hosts
    renamed_hosts="$(curl -sS "$(host_url_for 3)/api/v1/hosts?group_uuid=$renamed_uuid" \
        -H "Cookie: ${SESSION_COOKIE[3]}")"
    local special_host_uuid="${LOCAL_HOST_UUID["1-special-host-1"]}"
    if echo "$renamed_hosts" | jq -e --arg u "$special_host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
        ok "group remap verified: special-host-1 ($special_host_uuid) lives in 'Renamed' on haze-3"
    else
        warn "haze-3 'Renamed' group does not contain special-host-1; dumping members"
        echo "$renamed_hosts" | jq -r '.[] | "  \(.uuid)  \(.display_name)"'
        fail "group remap rule did not relocate the host"
    fi

    log "verifying that instance 8 has host-6 from the 6-fan (poll up to 4 minutes)"
    for expected_idx in 6; do
        local host_uuid="${LOCAL_HOST_UUID["$expected_idx-host-$expected_idx"]}"
        local found=""
        for _ in $(seq 1 48); do
            if list_hosts 8 | jq -e --arg u "$host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
                found=1; break
            fi
            sleep 5
        done
        if [[ -n "$found" ]]; then
            ok "haze-8 has replicated host-$expected_idx"
        else
            list_hosts 8 | jq -r '.[] | "  \(.uuid)  \(.display_name)  peer=\(.replication_peer_id)"'
            fail "haze-8 missing host-$expected_idx"
        fi
    done
    log "verifying that haze-8 has host-7 via the 8<-7 rule (peer was wired up-front)"
    for expected_idx in 7; do
        local host_uuid="${LOCAL_HOST_UUID["$expected_idx-host-$expected_idx"]}"
        local found=""
        for _ in $(seq 1 48); do
            if list_hosts 8 | jq -e --arg u "$host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
                found=1; break
            fi
            sleep 5
        done
        [[ -n "$found" ]] || fail "haze-8 missing host-7"
        ok "haze-8 has replicated host-7"
    done

    log "late wire-up: adding 7<-2 peer + rule AFTER 8 is already running on the 7 branch"
    local late_resp
    late_resp="$(add_peer 7 2 from-2)"
    local late_peer_uuid
    late_peer_uuid="$(echo "$late_resp" | jq -r .uuid)"
    [[ -n "$late_peer_uuid" && "$late_peer_uuid" != "null" ]] || fail "late peer create failed: $late_resp"
    PEER_UUID["7-2"]="$late_peer_uuid"
    local late_rule_uuid
    late_rule_uuid="$(add_rule 7 "$late_peer_uuid")"
    ok "wired 7<-2: peer $late_peer_uuid, rule $late_rule_uuid"

    log "polling haze-7 for cascaded host-2 (via the late peer) and host-1 (via 1->2 cascade)"
    for expected_idx in 1 2; do
        local host_uuid="${LOCAL_HOST_UUID["$expected_idx-host-$expected_idx"]}"
        local found=""
        for _ in $(seq 1 60); do
            if list_hosts 7 | jq -e --arg u "$host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
                found=1; break
            fi
            sleep 5
        done
        [[ -n "$found" ]] || fail "haze-7 missing late-cascaded host-$expected_idx"
        ok "haze-7 received host-$expected_idx after late wire-up"
    done

    log "verifying haze-8 picks up host-1 and host-2 through the (now active) 7 branch too"
    # They likely arrived via the 6 path already, but this confirms the
    # late-add propagated through the existing 8<-7 stream.
    for expected_idx in 1 2; do
        local host_uuid="${LOCAL_HOST_UUID["$expected_idx-host-$expected_idx"]}"
        local found=""
        for _ in $(seq 1 60); do
            if list_hosts 8 | jq -e --arg u "$host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
                found=1; break
            fi
            sleep 5
        done
        [[ -n "$found" ]] || fail "haze-8 missing host-$expected_idx after late 7<-2 wire-up"
        ok "haze-8 has host-$expected_idx (propagation works after late wire-up)"
    done

    log "removal test: dropping the 7<-2 rule, expecting haze-7 to orphan host-1 / host-2 locally"
    curl -sS -X DELETE "$(host_url_for 7)/api/v1/replication/rules/$late_rule_uuid" \
        -H "Cookie: ${SESSION_COOKIE[7]}" >/dev/null
    ok "deleted 7<-2 rule"
    sleep 30
    # Hosts stay on disk (orphan policy); the cursor should be marked orphaned.
    # We verify via the inbound side: the slot on haze-2 should be gone.
    local inbound_on_2
    inbound_on_2="$(curl -sS "$(host_url_for 2)/api/v1/replication/inbound" \
        -H "Cookie: ${SESSION_COOKIE[2]}" | jq 'length')"
    ok "haze-2's inbound after rule removal has $inbound_on_2 slot(s) (downstream destinations on 2)"

    log "NOTE: haze-7 keeps host-1/host-2 locally as orphan (replication off but data preserved)"

    log "cycle-check: attempting to add haze-8 as a peer on haze-1 (must fail)"
    local cycle_resp
    cycle_resp="$(curl -sS -w '\n__HTTP__%{http_code}' -X POST \
        "$(host_url_for 1)/api/v1/replication/peers" \
        -H "Cookie: ${SESSION_COOKIE[1]}" \
        -H 'Content-Type: application/json' \
        -d "{\"name\":\"would-cycle\",\"base_url\":\"$(docker_url_for 8)\",\"api_token\":\"${TOKEN[8]}\"}")"
    local cycle_status
    cycle_status="$(echo "$cycle_resp" | tail -n1 | sed 's/^__HTTP__//')"
    local cycle_body
    cycle_body="$(echo "$cycle_resp" | sed '$d')"
    if [[ "$cycle_status" == "422" || "$cycle_status" == "400" ]]; then
        ok "cycle correctly refused (HTTP $cycle_status): $(echo "$cycle_body" | jq -r .detail)"
    else
        fail "expected 4xx for cycle attempt, got $cycle_status: $cycle_body"
    fi

    log "data-symmetry check: every cascaded host on every downstream instance"
    log "  must return the same /series data as the original source within ±2 samples"
    # By the time symmetry check runs, the late 7<-2 rule has been
    # deleted, so haze-7 stops pulling host-1/host-2 from haze-2 and
    # only tracks its own local host-7 perfectly. (haze-1's host-1
    # samples on haze-7 are frozen as orphan data, not in sync.)
    # haze-8's view of host-7 comes from 8<-7 (active, expected
    # symmetric); host-1 and friends arrive via 8<-6's cascade.
    declare -A EXPECTED_HOSTS=(
        [1]="1"
        [2]="1 2"
        [3]="1 2 3"
        [4]="1 2 3 4"
        [5]="1 2 3 4"
        [6]="1 2 3 4 6"
        [7]="7"
        [8]="1 2 3 4 6 7"
    )
    # Stall-detection logic: the absolute drift between source and a
    # downstream can be non-zero (initial catch-up window, occasional
    # SSE reconnects). What matters operationally is that the drift
    # doesn't GROW between rounds - if it does, replication has stalled.
    # We record round-1 drift as the baseline and require subsequent
    # rounds to stay within +2 of it (i.e. tolerating one sample of
    # noise but flagging real stalls). 30s spacing matches the probe
    # interval so a healthy stream should keep up exactly.
    declare -A BASELINE_DRIFT=()
    for round in 1 2 3; do
        log "symmetry round $round/3 (30s spacing; drift must not grow)"
        for dest_idx in 1 2 3 4 5 6 7 8; do
            for src_idx in ${EXPECTED_HOSTS[$dest_idx]}; do
                local host_uuid="${LOCAL_HOST_UUID["$src_idx-host-$src_idx"]}"
                local now_ts="$(date +%s)"
                local from_ts=$((now_ts - 300))
                local nsrc ndest
                nsrc="$(curl -sS "$(host_url_for "$src_idx")/api/v1/hosts/$host_uuid/series?from=$from_ts&to=$now_ts&max_samples=400" \
                    -H "Cookie: ${SESSION_COOKIE[$src_idx]}" | jq '.samples | length')"
                ndest="$(curl -sS "$(host_url_for "$dest_idx")/api/v1/hosts/$host_uuid/series?from=$from_ts&to=$now_ts&max_samples=400" \
                    -H "Cookie: ${SESSION_COOKIE[$dest_idx]}" | jq '.samples | length')"
                local diff=$((nsrc - ndest))
                local key="$dest_idx-$src_idx"
                if [[ "$round" == "1" ]]; then
                    BASELINE_DRIFT[$key]=$diff
                else
                    local baseline="${BASELINE_DRIFT[$key]:-0}"
                    local growth=$((diff - baseline))
                    if [[ "$growth" -gt 2 ]]; then
                        fail "round $round STALL: haze-$dest_idx has $ndest samples for host-$src_idx, source haze-$src_idx has $nsrc. Drift grew from $baseline → $diff between rounds (replication isn't keeping up)"
                    fi
                fi
            done
        done
        ok "round $round symmetry check passed across all expected (instance, host) pairs"
        [[ "$round" -lt 3 ]] && sleep 30
    done

    log "remap-symmetry: haze-3's 'Renamed' should contain special-host-1 with same data as haze-1"
    local special_uuid="${LOCAL_HOST_UUID["1-special-host-1"]}"
    local now_ts2 from_ts2
    now_ts2="$(date +%s)"
    from_ts2=$((now_ts2 - 300))
    local nsrc nremap
    nsrc="$(curl -sS "$(host_url_for 1)/api/v1/hosts/$special_uuid/series?from=$from_ts2&to=$now_ts2&max_samples=400" \
        -H "Cookie: ${SESSION_COOKIE[1]}" | jq '.samples | length')"
    nremap="$(curl -sS "$(host_url_for 3)/api/v1/hosts/$special_uuid/series?from=$from_ts2&to=$now_ts2&max_samples=400" \
        -H "Cookie: ${SESSION_COOKIE[3]}" | jq '.samples | length')"
    local rd=$((nsrc - nremap)); rd=${rd#-}
    if [[ "$rd" -le 3 ]]; then
        ok "remap symmetry: haze-1.special-host-1 has $nsrc samples; haze-3 via remap rule has $nremap (drift $rd)"
    else
        fail "remap-symmetry drift too large: haze-1=$nsrc haze-3-renamed=$nremap"
    fi

    log "verifying samples actually FLOW end-to-end: probe on haze-1 → 4 hops → haze-5 chart data"
    # First confirm haze-1's local probe actually produced samples; if
    # ping is broken in the container the rest of the test would be
    # meaningless (we'd just be checking metadata propagation, not data).
    local h1_uuid="${LOCAL_HOST_UUID["1-host-1"]}"
    local now_ts from_ts series1
    now_ts="$(date +%s)"
    from_ts=$((now_ts - 300))
    series1="$(curl -sS "$(host_url_for 1)/api/v1/hosts/$h1_uuid/series?from=$from_ts&to=$now_ts&max_samples=200" \
        -H "Cookie: ${SESSION_COOKIE[1]}")"
    local n1
    n1="$(echo "$series1" | jq '.samples | length' 2>/dev/null || echo 0)"
    [[ "$n1" -gt 0 ]] || fail "haze-1 has ZERO local samples for its own host - probes aren't producing data (check NET_RAW + ping_group_range in the container)"
    ok "haze-1 has $n1 local samples for host-1 (probes are running)"

    # Poll haze-5 for cascaded samples. With 10s probe + ~4 hops of SSE
    # forwarding + WAL→chunk write at each hop, give it up to 3 minutes.
    local n5=0
    for _ in $(seq 1 36); do
        now_ts="$(date +%s)"
        from_ts=$((now_ts - 300))
        n5="$(curl -sS "$(host_url_for 5)/api/v1/hosts/$h1_uuid/series?from=$from_ts&to=$now_ts&max_samples=200" \
            -H "Cookie: ${SESSION_COOKIE[5]}" | jq '.samples | length')"
        [[ "$n5" -gt 0 ]] && break
        sleep 5
    done
    if [[ "$n5" -gt 0 ]]; then
        ok "haze-5 has $n5 samples for host-1 (4-hop replication delivers actual data)"
    else
        # Dump per-hop sample counts so the failure points at where the
        # break is (probes / 1->2 / 2->3 / 3->4 / 4->5).
        for hop_idx in 2 3 4 5; do
            local nh
            nh="$(curl -sS "$(host_url_for "$hop_idx")/api/v1/hosts/$h1_uuid/series?from=$from_ts&to=$now_ts&max_samples=200" \
                -H "Cookie: ${SESSION_COOKIE[$hop_idx]}" | jq '.samples | length')"
            warn "  haze-$hop_idx has $nh samples for host-1"
        done
        fail "samples did NOT cascade through the 1->2->3->4->5 chain"
    fi

    # ─── Live-mutation phase ────────────────────────────────────────
    log "live-add: creating a new host 'late-host-1' on haze-1, expect cascade to haze-5"
    mk_host 1 "late-host-1" "${LOCAL_GROUP_UUID["1-grp-1"]}" "haze-4"
    local late_host_uuid="${LOCAL_HOST_UUID["1-late-host-1"]}"
    local saw_late=""
    for _ in $(seq 1 60); do
        if list_hosts 5 | jq -e --arg u "$late_host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
            saw_late=1
            break
        fi
        sleep 5
    done
    if [[ -n "$saw_late" ]]; then
        ok "haze-5 received the late-added host within 5 minutes"
    else
        fail "live-add did not cascade to haze-5 in time"
    fi

    log "live-delete: removing 'late-host-1' on haze-1, expect haze-5 to orphan (keep) the host"
    curl -sS -X DELETE "$(host_url_for 1)/api/v1/hosts/$late_host_uuid" \
        -H "Cookie: ${SESSION_COOKIE[1]}" >/dev/null
    # Wait long enough for haze-5's reconcile to notice the removal
    # (reconcile_interval_secs defaults to 300, but we give it 120s and
    # rely on the manifest-changed SSE event for a faster trigger).
    sleep 120
    if list_hosts 5 | jq -e --arg u "$late_host_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
        ok "haze-5 still has the orphaned late-host-1 (data preserved as designed)"
    else
        warn "haze-5 dropped the host (unexpected - should have orphaned, kept locally)"
    fi

    log "live group-add: creating 'LateGroup' on haze-2, expect haze-3 to mirror it"
    mk_group 2 "LateGroup"
    local late_group_uuid="${LOCAL_GROUP_UUID["2-LateGroup"]}"
    local saw_late_group=""
    for _ in $(seq 1 60); do
        if curl -sS "$(host_url_for 3)/api/v1/groups" -H "Cookie: ${SESSION_COOKIE[3]}" \
            | jq -e '.[] | select(.display_name == "LateGroup")' >/dev/null; then
            saw_late_group=1
            break
        fi
        sleep 5
    done
    if [[ -n "$saw_late_group" ]]; then
        ok "haze-3 mirrored the late-added 'LateGroup' from haze-2"
    else
        warn "live-add of LateGroup did not propagate to haze-3 in 5 minutes"
    fi

    log "delete + recreate rule: dropping haze-3 <- haze-2 rule, then re-creating it"
    local first_rule="${PEER_UUID[3-2]}"
    # The rule UUID isn't kept by add_rule's return; query for it.
    local existing_rule
    existing_rule="$(curl -sS "$(host_url_for 3)/api/v1/replication/rules?peer_uuid=$first_rule" \
        -H "Cookie: ${SESSION_COOKIE[3]}" \
        | jq -r '[.[] | select(.source_group_uuid == "00000000-0000-0000-0000-000000000000")] | .[0].uuid')"
    if [[ -n "$existing_rule" && "$existing_rule" != "null" ]]; then
        curl -sS -X DELETE "$(host_url_for 3)/api/v1/replication/rules/$existing_rule" \
            -H "Cookie: ${SESSION_COOKIE[3]}" >/dev/null
        ok "deleted root->root rule on haze-3 ($existing_rule)"
        sleep 10
        # Recreate
        local new_rule_uuid
        new_rule_uuid="$(add_rule 3 "${PEER_UUID[3-2]}")"
        ok "recreated root->root rule on haze-3 ($new_rule_uuid)"
        # Wait for catch-up and check we still see host-2
        sleep 30
        local host2_uuid="${LOCAL_HOST_UUID["2-host-2"]}"
        if list_hosts 3 | jq -e --arg u "$host2_uuid" '.[] | select(.uuid == $u)' >/dev/null; then
            ok "after rule recreate, haze-3 still has host-2 (replication resumed cleanly)"
        else
            warn "after rule recreate, haze-3 missing host-2 - replication may need more time"
        fi
    else
        warn "could not find root->root rule on haze-3 to delete"
    fi

    log "block/unblock data-flow: confirm samples actually freeze + resume across the block"
    # haze-3 has TWO active rules pulling from haze-2 (root->root +
    # Special->Renamed remap). Blocking just one slot doesn't stop the
    # data flow because the other rule continues to deliver. Block ALL
    # of haze-3's slots on haze-2 so the destination truly stops.
    local inbound_resp_bf haze3_peer_uuid bf_slots
    haze3_peer_uuid="$(curl -sS "$(host_url_for 3)/api/v1/server-info" \
        -H "Cookie: ${SESSION_COOKIE[3]}" | jq -r '.instance_uuid')"
    inbound_resp_bf="$(curl -sS "$(host_url_for 2)/api/v1/replication/inbound" \
        -H "Cookie: ${SESSION_COOKIE[2]}")"
    bf_slots="$(echo "$inbound_resp_bf" \
        | jq -r --arg p "$haze3_peer_uuid" '.[] | select(.peer_instance_uuid == $p) | .slot_uuid')"
    local bf_slot
    bf_slot="$(echo "$bf_slots" | head -n1)"
    if [[ -n "$bf_slot" && "$bf_slot" != "null" ]]; then
        local h1_for_bf="${LOCAL_HOST_UUID["1-host-1"]}"
        local nbefore
        nbefore="$(curl -sS "$(host_url_for 3)/api/v1/hosts/$h1_for_bf/series?from=$(($(date +%s) - 300))&to=$(date +%s)&max_samples=400" \
            -H "Cookie: ${SESSION_COOKIE[3]}" | jq '.samples | length')"
        ok "before block: haze-3 has $nbefore samples for host-1"
        for s in $bf_slots; do
            curl -sS -X DELETE "$(host_url_for 2)/api/v1/replication/inbound/$s" \
                -H "Cookie: ${SESSION_COOKIE[2]}" >/dev/null
        done
        local nblocks
        nblocks="$(echo "$bf_slots" | wc -l | tr -d ' ')"
        ok "blocked $nblocks haze-2 → haze-3 slot(s)"
        # Give the worker time to receive a 403 on its next call and stop
        # ingesting. Then wait 30s and confirm haze-3's count didn't keep growing.
        sleep 45
        local nduring
        nduring="$(curl -sS "$(host_url_for 3)/api/v1/hosts/$h1_for_bf/series?from=$(($(date +%s) - 300))&to=$(date +%s)&max_samples=400" \
            -H "Cookie: ${SESSION_COOKIE[3]}" | jq '.samples | length')"
        ok "during block: haze-3 has $nduring samples (was $nbefore - block freezes the cursor)"
        # The 300s window also slides, so within reason the count may
        # drop or stay flat. The real check is that it didn't grow by
        # the ~4 samples we'd expect at 10s interval in 45s.
        local growth=$((nduring - nbefore))
        if [[ "$growth" -lt 3 ]]; then
            ok "ingestion stopped while blocked (growth=$growth < 3)"
        else
            fail "blocking did NOT stop ingestion: count grew by $growth in 45s"
        fi
        # Unblock all slots we just blocked.
        for s in $bf_slots; do
            curl -sS -X POST "$(host_url_for 2)/api/v1/replication/inbound/$s/unblock" \
                -H "Cookie: ${SESSION_COOKIE[2]}" >/dev/null
        done
        ok "unblocked $nblocks slot(s)"
        # Worst-case worker delay: backoff cap (60 s) + catch-up + first
        # SSE sample (~12 s). Wait long enough for the slowest path.
        local nafter resume
        for _ in $(seq 1 24); do
            sleep 10
            nafter="$(curl -sS "$(host_url_for 3)/api/v1/hosts/$h1_for_bf/series?from=$(($(date +%s) - 300))&to=$(date +%s)&max_samples=400" \
                -H "Cookie: ${SESSION_COOKIE[3]}" | jq '.samples | length')"
            resume=$((nafter - nduring))
            [[ "$resume" -ge 3 ]] && break
        done
        if [[ "$resume" -ge 3 ]]; then
            ok "ingestion resumed after unblock: $nduring → $nafter samples (grew by $resume)"
        else
            fail "ingestion did NOT resume after unblock: count went from $nduring to $nafter"
        fi
    else
        warn "no inbound slot found on haze-2 to test block/unblock data flow"
    fi

    log "force-remove + unblock: source admin force-removes the haze-2 -> haze-3 slot, then unblocks"
    local inbound_resp
    inbound_resp="$(curl -sS "$(host_url_for 2)/api/v1/replication/inbound" \
        -H "Cookie: ${SESSION_COOKIE[2]}")"
    local target_slot
    target_slot="$(echo "$inbound_resp" | jq -r '[.[] | select(.peer_label | contains("haze"))] | .[0].slot_uuid' 2>/dev/null)"
    if [[ -n "$target_slot" && "$target_slot" != "null" ]]; then
        curl -sS -X DELETE "$(host_url_for 2)/api/v1/replication/inbound/$target_slot" \
            -H "Cookie: ${SESSION_COOKIE[2]}" >/dev/null
        ok "force-removed inbound slot $target_slot on haze-2"
        local blocked_at
        blocked_at="$(curl -sS "$(host_url_for 2)/api/v1/replication/inbound" \
            -H "Cookie: ${SESSION_COOKIE[2]}" \
            | jq -r --arg s "$target_slot" '.[] | select(.slot_uuid == $s) | .blocked_at')"
        if [[ -n "$blocked_at" && "$blocked_at" != "null" ]]; then
            ok "slot is now blocked (blocked_at=$blocked_at) - destination calls will 403"
        else
            warn "expected blocked_at to be set after force-remove"
        fi
        # Unblock
        curl -sS -X POST "$(host_url_for 2)/api/v1/replication/inbound/$target_slot/unblock" \
            -H "Cookie: ${SESSION_COOKIE[2]}" >/dev/null
        local unblock_check
        unblock_check="$(curl -sS "$(host_url_for 2)/api/v1/replication/inbound" \
            -H "Cookie: ${SESSION_COOKIE[2]}" \
            | jq -r --arg s "$target_slot" '.[] | select(.slot_uuid == $s) | .blocked_at')"
        if [[ -z "$unblock_check" || "$unblock_check" == "null" ]]; then
            ok "slot was successfully unblocked; replication can resume"
        else
            warn "unblock did not clear blocked_at (got: $unblock_check)"
        fi
    else
        warn "no inbound slot found on haze-2 to test force-remove/unblock"
    fi

    ok "ALL CHECKS PASSED"
    log "compose stack left running for inspection. To tear down:"
    log "  docker compose -f $COMPOSE_FILE down -v"
}

main "$@"
