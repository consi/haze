#!/usr/bin/env python3
"""Populate a Haze instance with curated test hosts and nested groups.

The host list is hand-curated and pre-verified (DNS, ping, TCP, TLS, HTTPS were
checked manually against each entry). The script just POSTs the list; it does
not preflight network reachability at runtime.

Usage:
    python3 seed_haze.py --url http://localhost:5173 --token hzt_...

Re-running is safe: existing groups (matched by parent+name) and existing hosts
(matched by display_name) are skipped.
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from typing import Any


# ---------- API client ---------------------------------------------------------


class HazeClient:
    def __init__(self, base_url: str, token: str, dry_run: bool = False):
        self.base = base_url.rstrip("/") + "/api/v1"
        self.token = token
        self.dry_run = dry_run

    def _request(self, method: str, path: str, body: Any = None) -> tuple[int, Any]:
        url = self.base + path
        data = None
        headers = {"Authorization": f"Bearer {self.token}", "Accept": "application/json"}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                payload = resp.read()
                return resp.status, json.loads(payload) if payload else None
        except urllib.error.HTTPError as e:
            payload = e.read()
            try:
                return e.code, json.loads(payload) if payload else None
            except json.JSONDecodeError:
                return e.code, payload.decode("utf-8", errors="replace")

    def list_groups(self) -> list[dict]:
        status, body = self._request("GET", "/groups")
        if status != 200:
            raise RuntimeError(f"GET /groups -> {status}: {body}")
        return body

    def list_hosts(self) -> list[dict]:
        status, body = self._request("GET", "/hosts")
        if status != 200:
            raise RuntimeError(f"GET /hosts -> {status}: {body}")
        return body

    def create_group(self, display_name: str, parent_uuid: str | None) -> tuple[int, Any]:
        if self.dry_run:
            return 201, {"uuid": f"dryrun-group-{display_name}", "parent_uuid": parent_uuid}
        return self._request("POST", "/groups", {
            "display_name": display_name,
            "parent_uuid": parent_uuid,
        })

    def create_host(self, body: dict) -> tuple[int, Any]:
        if self.dry_run:
            return 201, {"uuid": f"dryrun-host-{body['display_name']}"}
        return self._request("POST", "/hosts", body)


# ---------- Group tree --------------------------------------------------------


@dataclass
class HostSpec:
    display_name: str
    probe_type: str
    probe_config: dict


@dataclass
class GroupNode:
    name: str
    children: list["GroupNode"] = field(default_factory=list)
    hosts: list[HostSpec] = field(default_factory=list)

    def group(self, name: str) -> "GroupNode":
        child = GroupNode(name)
        self.children.append(child)
        return child

    def host(self, display_name: str, probe_type: str, **config: Any) -> None:
        self.hosts.append(HostSpec(display_name, probe_type, config))

    def ping_many(self, prefix: str, targets: list[tuple[str, str]]) -> None:
        for label, tgt in targets:
            self.host(f"{prefix} - {label}", "ping", target=tgt)

    def tcp_many(self, prefix: str, targets: list[tuple[str, str]], port: int) -> None:
        for label, host in targets:
            self.host(f"{prefix} - {label}", "tcp_connect", host=host, port=port)

    def tls_many(self, prefix: str, targets: list[tuple[str, str]], port: int = 443) -> None:
        for label, host in targets:
            self.host(f"{prefix} - {label}", "tls_connect", host=host, port=port)

    def http_many(self, prefix: str, targets: list[tuple[str, str]], follow: bool = True,
                  total: bool = False, expect: str = "2xx") -> None:
        probe = "http_total" if total else "http_ttfb"
        for label, url in targets:
            cfg = {"url": url, "follow_redirects": follow, "expect_status": expect}
            if total:
                cfg["max_bytes"] = 131072
            self.host(f"{prefix} - {label}", probe, **cfg)


def build_tree() -> GroupNode:
    root = GroupNode("__ROOT__")

    # ---------- Public DNS Resolvers ------------------------------------------
    dns_root = root.group("Public DNS Resolvers")

    dns_ipv4_resolvers: list[tuple[str, str]] = [
        ("Google Primary", "8.8.8.8"),
        ("Google Secondary", "8.8.4.4"),
        ("Cloudflare Primary", "1.1.1.1"),
        ("Cloudflare Secondary", "1.0.0.1"),
        ("Quad9 Primary", "9.9.9.9"),
        ("Quad9 Secondary", "149.112.112.112"),
        ("OpenDNS Primary", "208.67.222.222"),
        ("OpenDNS Secondary", "208.67.220.220"),
        ("AdGuard Primary", "94.140.14.14"),
        ("AdGuard Secondary", "94.140.15.15"),
        ("CleanBrowsing Security", "185.228.168.9"),
        ("Comodo Secure DNS", "8.26.56.26"),
        ("Level3 Primary", "4.2.2.1"),
        ("Level3 Secondary", "4.2.2.2"),
        ("ControlD Primary", "76.76.2.0"),
        ("ControlD Secondary", "76.76.10.0"),
        ("SafeDNS Primary", "195.46.39.39"),
        ("SafeDNS Secondary", "195.46.39.40"),
    ]
    dns_query_group = dns_root.group("IPv4 resolvers - DNS query (A example.com)")
    for label, ip in dns_ipv4_resolvers:
        dns_query_group.host(f"DNS A {label}", "dns",
                             query="example.com", record_type="A",
                             resolver=f"{ip}:53")

    dns_query_aaaa = dns_root.group("IPv4 resolvers - DNS query (AAAA cloudflare.com)")
    for label, ip in dns_ipv4_resolvers[:8]:
        dns_query_aaaa.host(f"DNS AAAA {label}", "dns",
                            query="cloudflare.com", record_type="AAAA",
                            resolver=f"{ip}:53")

    dns_query_mx = dns_root.group("IPv4 resolvers - DNS query (MX gmail.com)")
    for label, ip in dns_ipv4_resolvers[:6]:
        dns_query_mx.host(f"DNS MX {label}", "dns",
                          query="gmail.com", record_type="MX",
                          resolver=f"{ip}:53")

    # Ping the same IPs (drop the ones that don't reply to ICMP)
    pingable = [t for t in dns_ipv4_resolvers if t[1] not in {"8.26.56.26"}]
    dns_ping_group = dns_root.group("IPv4 resolvers - ICMP ping")
    for label, ip in pingable:
        dns_ping_group.host(f"PING {label}", "ping", target=ip)

    dns_dot_group = dns_root.group("DNS-over-TLS endpoints (TLS connect :853)")
    for label, host in [
        ("Google DoT", "dns.google"),
        ("Cloudflare DoT", "one.one.one.one"),
        ("Quad9 DoT", "dns.quad9.net"),
        ("AdGuard DoT", "dns.adguard-dns.com"),
        ("AdGuard Unfiltered DoT", "unfiltered.adguard-dns.com"),
    ]:
        dns_dot_group.host(f"DoT {label}", "tls_connect", host=host, port=853)

    dns_doh_group = dns_root.group("DNS-over-HTTPS endpoints (HTTPS TTFB)")
    for label, url in [
        ("Cloudflare DoH", "https://cloudflare-dns.com/"),
        ("Google DoH portal", "https://dns.google/"),
        ("Quad9 DoH portal", "https://www.quad9.net/"),
        ("OpenDNS portal", "https://www.opendns.com/"),
        ("AdGuard DoH portal", "https://adguard-dns.io/"),
    ]:
        dns_doh_group.host(f"DoH {label}", "http_ttfb", url=url,
                           follow_redirects=True, expect_status="2xx")

    # ---------- Cloud Providers -----------------------------------------------
    cloud_root = root.group("Cloud Providers")

    aws_root = cloud_root.group("AWS")
    aws_regions = [
        ("us-east-1 N. Virginia", "ec2.us-east-1.amazonaws.com"),
        ("us-east-2 Ohio", "ec2.us-east-2.amazonaws.com"),
        ("us-west-1 N. California", "ec2.us-west-1.amazonaws.com"),
        ("us-west-2 Oregon", "ec2.us-west-2.amazonaws.com"),
        ("eu-west-1 Ireland", "ec2.eu-west-1.amazonaws.com"),
        ("eu-west-2 London", "ec2.eu-west-2.amazonaws.com"),
        ("eu-west-3 Paris", "ec2.eu-west-3.amazonaws.com"),
        ("eu-central-1 Frankfurt", "ec2.eu-central-1.amazonaws.com"),
        ("eu-north-1 Stockholm", "ec2.eu-north-1.amazonaws.com"),
        ("ap-southeast-1 Singapore", "ec2.ap-southeast-1.amazonaws.com"),
        ("ap-southeast-2 Sydney", "ec2.ap-southeast-2.amazonaws.com"),
        ("ap-northeast-1 Tokyo", "ec2.ap-northeast-1.amazonaws.com"),
        ("ap-northeast-2 Seoul", "ec2.ap-northeast-2.amazonaws.com"),
        ("ap-south-1 Mumbai", "ec2.ap-south-1.amazonaws.com"),
        ("sa-east-1 Sao Paulo", "ec2.sa-east-1.amazonaws.com"),
        ("ca-central-1 Canada", "ec2.ca-central-1.amazonaws.com"),
        ("af-south-1 Cape Town", "ec2.af-south-1.amazonaws.com"),
    ]
    aws_pingable = {  # endpoints that actually respond to ICMP
        "ec2.us-east-2.amazonaws.com", "ec2.us-west-1.amazonaws.com",
        "ec2.us-west-2.amazonaws.com", "ec2.eu-west-1.amazonaws.com",
        "ec2.eu-west-2.amazonaws.com", "ec2.eu-central-1.amazonaws.com",
        "ec2.eu-north-1.amazonaws.com", "ec2.ap-southeast-1.amazonaws.com",
        "ec2.ap-southeast-2.amazonaws.com", "ec2.ap-northeast-1.amazonaws.com",
        "ec2.ap-northeast-2.amazonaws.com", "ec2.ap-south-1.amazonaws.com",
        "ec2.sa-east-1.amazonaws.com", "ec2.af-south-1.amazonaws.com",
    }
    aws_ping_grp = aws_root.group("EC2 regional endpoints - ICMP ping")
    for label, host in aws_regions:
        if host in aws_pingable:
            aws_ping_grp.host(f"AWS ping {label}", "ping", target=host)

    aws_tcp_grp = aws_root.group("EC2 regional endpoints - TCP :443")
    for label, host in aws_regions:
        aws_tcp_grp.host(f"AWS TCP :443 {label}", "tcp_connect", host=host, port=443)

    aws_s3_grp = aws_root.group("S3 regional endpoints - TLS :443")
    for label, host in aws_regions:
        s3 = host.replace("ec2.", "s3.")
        aws_s3_grp.host(f"AWS S3 TLS {label}", "tls_connect", host=s3, port=443)

    gcp_root = cloud_root.group("Google Cloud")
    gcp_buckets = [
        ("US multi-region", "us.storage.googleapis.com"),
        ("EU multi-region", "eu.storage.googleapis.com"),
        ("Asia multi-region", "asia.storage.googleapis.com"),
    ]
    gcp_root.group("Storage - Ping").ping_many("GCP storage ping", gcp_buckets)
    gcp_root.group("Storage - TLS :443").tls_many("GCP storage TLS", gcp_buckets)
    gcp_http = gcp_root.group("Storage - HTTPS TTFB")
    for label, host in gcp_buckets:
        gcp_http.host(f"GCP storage HTTPS {label}", "http_ttfb",
                      url=f"https://{host}/", follow_redirects=True,
                      expect_status="400")  # GET / on bucket root returns 400

    azure_root = cloud_root.group("Azure")
    azure_regions = [
        ("East US", "eastus.blob.core.windows.net"),
        ("West US 2", "westus2.blob.core.windows.net"),
        ("North Europe", "northeurope.blob.core.windows.net"),
        ("Southeast Asia", "southeastasia.blob.core.windows.net"),
        ("Brazil South", "brazilsouth.blob.core.windows.net"),
        ("Central India", "centralindia.blob.core.windows.net"),
        ("UK South", "uksouth.blob.core.windows.net"),
    ]
    azure_root.group("Blob - TCP :443").tcp_many("Azure blob TCP", azure_regions, 443)
    azure_root.group("Blob - TLS :443").tls_many("Azure blob TLS", azure_regions)

    linode_root = cloud_root.group("Linode (Akamai)")
    linode_regions = [
        ("Newark", "speedtest.newark.linode.com"),
        ("Atlanta", "speedtest.atlanta.linode.com"),
        ("Dallas", "speedtest.dallas.linode.com"),
        ("Fremont", "speedtest.fremont.linode.com"),
        ("Toronto", "speedtest.toronto1.linode.com"),
        ("London", "speedtest.london.linode.com"),
        ("Frankfurt", "speedtest.frankfurt.linode.com"),
        ("Singapore", "speedtest.singapore.linode.com"),
        ("Tokyo", "speedtest.tokyo2.linode.com"),
        ("Sydney", "speedtest.sydney.linode.com"),
    ]
    linode_root.group("Speedtest hosts - Ping").ping_many("Linode ping", linode_regions)
    linode_root.group("Speedtest hosts - TCP :443").tcp_many("Linode TCP", linode_regions, 443)

    vultr_root = cloud_root.group("Vultr")
    vultr_ips = [
        ("Atlanta", "108.61.193.166"),
        ("Dallas", "108.61.224.175"),
        ("Los Angeles", "108.61.219.200"),
        ("Miami", "104.156.244.232"),
        ("New York", "108.61.149.182"),
        ("Seattle", "108.61.194.105"),
        ("Silicon Valley", "104.156.230.107"),
        ("Amsterdam", "108.61.198.102"),
        ("Frankfurt", "108.61.210.117"),
        ("London", "108.61.196.101"),
        ("Paris", "108.61.209.127"),
        ("Sao Paulo", "216.238.98.118"),
        ("Sydney", "108.61.212.117"),
        ("Tokyo", "108.61.201.151"),
        ("Singapore", "45.32.100.168"),
        ("Seoul", "141.164.34.61"),
    ]
    vultr_root.group("Speedtest IPs - Ping").ping_many("Vultr ping", vultr_ips)
    vultr_root.group("Speedtest IPs - TCP :443").tcp_many("Vultr TCP", vultr_ips, 443)

    hetzner_root = cloud_root.group("Hetzner")
    hetzner_dcs = [
        ("Falkenstein FSN1", "fsn1-speed.hetzner.com"),
        ("Nuremberg NBG1", "nbg1-speed.hetzner.com"),
        ("Helsinki HEL1", "hel1-speed.hetzner.com"),
        ("Ashburn ASH", "ash-speed.hetzner.com"),
        ("Hillsboro HIL", "hil-speed.hetzner.com"),
        ("Singapore SIN", "sin-speed.hetzner.com"),
    ]
    hetzner_root.group("Speedtest - Ping").ping_many("Hetzner ping", hetzner_dcs)
    hetzner_root.group("Speedtest - TLS :443").tls_many("Hetzner TLS", hetzner_dcs)

    ovh_root = cloud_root.group("OVH")
    ovh_dcs = [
        ("OVH global", "proof.ovh.net"),
        ("OVH US", "proof.ovh.us"),
        ("Gravelines GRA", "gra.proof.ovh.net"),
        ("Strasbourg SBG", "sbg.proof.ovh.net"),
        ("Singapore SGP", "sgp.proof.ovh.net"),
        ("Sydney SYD", "syd.proof.ovh.net"),
    ]
    ovh_root.group("Proof - Ping").ping_many("OVH ping", ovh_dcs)
    ovh_root.group("Proof - TCP :443").tcp_many("OVH TCP", ovh_dcs, 443)

    # ---------- Major Websites & CDNs -----------------------------------------
    web_root = root.group("Major Websites & CDNs")

    search_sites = [
        ("Google", "google.com"),
        ("Bing", "bing.com"),
        ("DuckDuckGo", "duckduckgo.com"),
        ("Yahoo", "yahoo.com"),
        ("Baidu", "baidu.com"),
        ("Yandex", "yandex.com"),
        ("Brave Search", "brave.com"),
    ]
    web_search = web_root.group("Search & Portals")
    web_search.group("HTTPS TTFB").http_many(
        "Search HTTPS",
        [(n, f"https://{h}/") for n, h in search_sites], follow=True)
    web_search.group("HTTPS download (total)").http_many(
        "Search download",
        [(n, f"https://{h}/") for n, h in search_sites], follow=True, total=True)
    web_search.group("TLS :443").tls_many("Search TLS", search_sites)
    web_search.group("Ping").ping_many("Search ping", search_sites)

    social_sites = [
        ("Facebook", "facebook.com"),
        ("Instagram", "instagram.com"),
        ("LinkedIn", "linkedin.com"),
        ("Reddit", "reddit.com"),
        ("YouTube", "youtube.com"),
        ("TikTok", "tiktok.com"),
        ("Pinterest", "pinterest.com"),
        ("Tumblr", "tumblr.com"),
        ("Vimeo", "vimeo.com"),
    ]
    web_social = web_root.group("Social & Video")
    web_social.group("HTTPS TTFB").http_many(
        "Social HTTPS",
        [(n, f"https://{h}/") for n, h in social_sites], follow=True)
    web_social.group("TLS :443").tls_many("Social TLS", social_sites + [
        ("Twitter", "twitter.com"), ("X.com", "x.com"), ("Twitch", "twitch.tv"),
    ])
    web_social.group("Ping").ping_many("Social ping", social_sites + [
        ("Twitter", "twitter.com"), ("Twitch", "twitch.tv"),
    ])

    ecom_sites = [
        ("eBay", "ebay.com"),
        ("Shopify", "shopify.com"),
        ("Walmart", "walmart.com"),
        ("Target", "target.com"),
        ("Best Buy", "bestbuy.com"),
        ("AliExpress", "aliexpress.com"),
    ]
    web_ecom = web_root.group("E-commerce")
    web_ecom.group("HTTPS TTFB").http_many(
        "E-com HTTPS",
        [(n, f"https://{h}/") for n, h in ecom_sites], follow=True)
    web_ecom.group("TLS :443").tls_many("E-com TLS", ecom_sites + [
        ("Amazon", "amazon.com"), ("Etsy", "etsy.com"),
        ("Mercado Libre", "mercadolibre.com"),
    ])

    dev_sites = [
        ("GitHub", "github.com"),
        ("GitLab", "gitlab.com"),
        ("Bitbucket", "bitbucket.org"),
        ("Stack Overflow", "stackoverflow.com"),
        ("PyPI", "pypi.org"),
        ("RubyGems", "rubygems.org"),
        ("Packagist", "packagist.org"),
        ("Docker Hub", "hub.docker.com"),
        ("Kubernetes", "kubernetes.io"),
        ("Golang", "golang.org"),
        ("Rust", "rust-lang.org"),
        ("Python", "python.org"),
        ("Node.js", "nodejs.org"),
    ]
    web_dev = web_root.group("Developer Infrastructure")
    web_dev.group("HTTPS TTFB").http_many(
        "Dev HTTPS",
        [(n, f"https://{h}/") for n, h in dev_sites], follow=True)
    web_dev.group("HTTPS download (total)").http_many(
        "Dev download",
        [(n, f"https://{h}/") for n, h in dev_sites], follow=True, total=True)
    web_dev.group("TLS :443").tls_many("Dev TLS", dev_sites + [
        ("npm Registry", "npmjs.com"),
        ("crates.io", "crates.io"),
        ("docker.io", "docker.io"),
    ])

    news_sites = [
        ("New York Times", "nytimes.com"),
        ("BBC", "bbc.com"),
        ("CNN", "cnn.com"),
        ("The Guardian", "theguardian.com"),
        ("Al Jazeera", "aljazeera.com"),
    ]
    web_news = web_root.group("News")
    web_news.group("HTTPS TTFB").http_many(
        "News HTTPS",
        [(n, f"https://{h}/") for n, h in news_sites], follow=True)
    web_news.group("TLS :443").tls_many("News TLS", news_sites + [
        ("Reuters", "reuters.com"), ("Bloomberg", "bloomberg.com"),
        ("WSJ", "wsj.com"), ("Washington Post", "washingtonpost.com"),
        ("Economist", "economist.com"),
    ])

    streaming_sites = [
        ("Hulu", "hulu.com"),
        ("Spotify", "spotify.com"),
        ("SoundCloud", "soundcloud.com"),
        ("Disney", "disney.com"),
        ("HBO Max", "hbomax.com"),
        ("Paramount+", "paramountplus.com"),
        ("Peacock", "peacocktv.com"),
    ]
    web_stream = web_root.group("Streaming Media")
    web_stream.group("HTTPS TTFB").http_many(
        "Streaming HTTPS",
        [(n, f"https://{h}/") for n, h in streaming_sites], follow=True)
    web_stream.group("TLS :443").tls_many("Streaming TLS", streaming_sites + [
        ("Netflix", "netflix.com"), ("Prime Video", "primevideo.com"),
        ("Apple TV", "appletv.com"),
    ])

    cdn_sites = [
        ("Cloudflare", "cloudflare.com"),
        ("Fastly", "fastly.com"),
        ("jsDelivr", "cdn.jsdelivr.net"),
        ("unpkg", "unpkg.com"),
        ("BunnyCDN", "bunnycdn.com"),
        ("CDN77", "cdn77.com"),
        ("StackPath", "stackpath.com"),
        ("KeyCDN", "keycdn.com"),
    ]
    web_cdn = web_root.group("CDN edges")
    web_cdn.group("HTTPS TTFB").http_many(
        "CDN HTTPS",
        [(n, f"https://{h}/") for n, h in cdn_sites], follow=True)
    web_cdn.group("TLS :443").tls_many("CDN TLS", cdn_sites + [
        ("Akamai", "akamai.com"), ("CDNetworks", "cdnetworks.com"),
    ])
    web_cdn.group("Ping").ping_many("CDN ping", cdn_sites + [
        ("Akamai", "akamai.com"),
    ])

    # ---------- Looking Glass / Network Operators -----------------------------
    lg_root = root.group("Looking Glass / Network Operators")

    lg_tier1 = [
        ("Hurricane Electric", "lg.he.net"),
        ("Telia", "lg.telia.net"),
        ("Zayo", "lg.zayo.com"),
        ("RETN", "lg.retn.net"),
        ("Lookingglass.io", "lookingglass.io"),
    ]
    lg_root.group("Tier-1 backbones - HTTPS TTFB").http_many(
        "LG HTTPS", [(n, f"https://{h}/") for n, h in lg_tier1], follow=True)
    lg_root.group("Tier-1 backbones - Ping").ping_many("LG ping", [
        (n, h) for n, h in lg_tier1 if h != "lookingglass.centurylink.com"
    ])
    lg_root.group("Tier-1 backbones - TLS :443").tls_many("LG TLS",
        lg_tier1 + [("CenturyLink/Lumen", "lookingglass.centurylink.com")])

    ixp_sites = [
        ("DE-CIX", "www.de-cix.net"),
        ("AMS-IX", "www.ams-ix.net"),
        ("LINX", "www.linx.net"),
        ("JPNAP", "www.jpnap.net"),
        ("France-IX", "www.france-ix.net"),
        ("Netnod", "www.netnod.se"),
    ]
    lg_root.group("IXPs - HTTPS TTFB").http_many(
        "IXP HTTPS", [(n, f"https://{h}/") for n, h in ixp_sites], follow=True)
    lg_root.group("IXPs - TLS :443").tls_many("IXP TLS", ixp_sites + [
        ("Equinix", "www.equinix.com"), ("MSK-IX", "www.msk-ix.ru"),
    ])

    speed_tools = [
        ("Cloudflare speed", "speed.cloudflare.com"),
        ("Speedtest.net", "speedtest.net"),
        ("Fast.com", "fast.com"),
        ("Ping.eu", "ping.eu"),
        ("Ping.pe", "ping.pe"),
        ("RIPE Atlas", "atlas.ripe.net"),
        ("Clouvider NYC", "nyc.speedtest.clouvider.net"),
    ]
    lg_root.group("Speed/latency tools - HTTPS TTFB").http_many(
        "Speedtools HTTPS",
        [(n, f"https://{h}/") for n, h in speed_tools if n != "Clouvider NYC"],
        follow=True)
    lg_root.group("Speed/latency tools - Ping").ping_many("Speedtools ping", [
        (n, h) for n, h in speed_tools if n != "RIPE Atlas"
    ])
    lg_root.group("Speed/latency tools - TLS :443").tls_many("Speedtools TLS", speed_tools)

    # ---------- Shared Hosting Providers --------------------------------------
    host_root = root.group("Shared Hosting Providers")

    hosting_us = [
        ("DreamHost", "dreamhost.com"),
        ("SiteGround", "siteground.com"),
        ("A2 Hosting", "a2hosting.com"),
        ("InMotion Hosting", "inmotionhosting.com"),
        ("Hostinger", "hostinger.com"),
        ("GreenGeeks", "greengeeks.com"),
        ("WP Engine", "wpengine.com"),
        ("Kinsta", "kinsta.com"),
        ("Flywheel", "flywheel.com"),
        ("Pressable", "pressable.com"),
        ("Liquid Web", "liquidweb.com"),
    ]
    hosting_us_403 = [
        ("HostGator", "hostgator.com"),
        ("Bluehost", "bluehost.com"),
        ("GoDaddy", "godaddy.com"),
        ("Namecheap", "namecheap.com"),
        ("iPage", "ipage.com"),
        ("Nexcess", "nexcess.net"),
        ("Cloudways", "cloudways.com"),
    ]
    host_us = host_root.group("US providers")
    host_us.group("HTTPS TTFB").http_many(
        "US hosting HTTPS",
        [(n, f"https://{h}/") for n, h in hosting_us], follow=True)
    host_us.group("TLS :443").tls_many("US hosting TLS", hosting_us + hosting_us_403)
    host_us.group("Ping").ping_many("US hosting ping", hosting_us + hosting_us_403)

    hosting_eu = [
        ("OVH", "ovh.com"),
        ("Hetzner", "hetzner.com"),
        ("IONOS", "ionos.com"),
        ("Strato", "strato.com"),
        ("Mittwald", "mittwald.de"),
        ("All-Inkl", "all-inkl.com"),
        ("Servage", "servage.net"),
        ("Contabo", "contabo.com"),
        ("Netcup", "netcup.de"),
        ("Scaleway", "scaleway.com"),
    ]
    host_eu = host_root.group("EU providers")
    host_eu.group("HTTPS TTFB").http_many(
        "EU hosting HTTPS",
        [(n, f"https://{h}/") for n, h in hosting_eu], follow=True)
    host_eu.group("TLS :443").tls_many("EU hosting TLS",
        hosting_eu + [("UpCloud", "upcloud.com")])
    host_eu.group("Ping").ping_many("EU hosting ping",
        hosting_eu + [("UpCloud", "upcloud.com")])

    hosting_apac = [
        ("Sakura Internet", "sakura.ad.jp"),
        ("GMO", "gmo.jp"),
        ("Tencent Cloud", "tencentcloud.com"),
    ]
    host_apac = host_root.group("APAC providers")
    host_apac.group("HTTPS TTFB").http_many(
        "APAC hosting HTTPS",
        [(n, f"https://{h}/") for n, h in hosting_apac], follow=True)
    host_apac.group("TLS :443").tls_many("APAC hosting TLS",
        hosting_apac + [("Alibaba Cloud", "alibabacloud.com"),
                        ("Naver", "naver.com"), ("Kakao", "kakaocorp.com")])

    # ---------- ISPs / Eyeball Networks ---------------------------------------
    isp_root = root.group("ISPs / Eyeball Networks")

    isp_na = [
        ("AT&T", "att.com"),
        ("Verizon", "verizon.com"),
        ("Cox", "cox.com"),
        ("Rogers", "rogers.com"),
        ("Bell Canada", "bell.ca"),
        ("T-Mobile", "t-mobile.com"),
    ]
    isp_na_extras = [
        ("Spectrum", "spectrum.com"),
        ("TELUS", "telus.com"),
        ("Xfinity", "xfinity.com"),
    ]
    isp_na_grp = isp_root.group("North America")
    isp_na_grp.group("HTTPS TTFB").http_many(
        "ISP-NA HTTPS",
        [(n, f"https://{h}/") for n, h in isp_na], follow=True)
    isp_na_grp.group("TLS :443").tls_many("ISP-NA TLS", isp_na + isp_na_extras)
    isp_na_grp.group("Ping").ping_many("ISP-NA ping", [
        ("AT&T", "att.com"), ("Verizon", "verizon.com"),
        ("Spectrum", "spectrum.com"), ("Cox", "cox.com"),
        ("Rogers", "rogers.com"), ("TELUS", "telus.com"),
        ("T-Mobile", "t-mobile.com"),
    ])

    isp_eu_https = [
        ("Deutsche Telekom", "telekom.de"),
        ("BT", "bt.com"),
        ("Vodafone", "vodafone.com"),
        ("Telefónica", "telefonica.com"),
        ("KPN", "kpn.com"),
        ("Telenor", "telenor.com"),
        ("Telia", "telia.com"),
        ("Freenet", "freenet.de"),
        ("Proximus", "proximus.be"),
    ]
    isp_eu_grp = isp_root.group("Europe")
    isp_eu_grp.group("HTTPS TTFB").http_many(
        "ISP-EU HTTPS",
        [(n, f"https://{h}/") for n, h in isp_eu_https], follow=True)
    isp_eu_grp.group("TLS :443").tls_many("ISP-EU TLS",
        isp_eu_https + [("Orange", "orange.com")])
    isp_eu_grp.group("Ping").ping_many("ISP-EU ping", [
        ("BT", "bt.com"), ("Orange", "orange.com"),
        ("Telefónica", "telefonica.com"), ("KPN", "kpn.com"),
        ("Telenor", "telenor.com"), ("Freenet", "freenet.de"),
    ])

    isp_apac_https = [
        ("KDDI", "kddi.com"),
        ("SingTel", "singtel.com"),
        ("Telstra", "telstra.com.au"),
        ("SK Telecom", "sktelecom.com"),
    ]
    isp_apac_extras = [
        ("NTT", "ntt.com"),
        ("Optus", "optus.com.au"),
        ("SoftBank", "softbank.jp"),
        ("KT", "kt.com"),
        ("Chunghwa Telecom", "chunghwa.com.tw"),
    ]
    isp_apac_grp = isp_root.group("Asia-Pacific")
    isp_apac_grp.group("HTTPS TTFB").http_many(
        "ISP-APAC HTTPS",
        [(n, f"https://{h}/") for n, h in isp_apac_https], follow=True)
    isp_apac_grp.group("TLS :443").tls_many("ISP-APAC TLS",
        isp_apac_https + isp_apac_extras)
    isp_apac_grp.group("Ping").ping_many("ISP-APAC ping", [
        ("NTT", "ntt.com"), ("KDDI", "kddi.com"),
        ("SingTel", "singtel.com"), ("Telstra", "telstra.com.au"),
        ("Optus", "optus.com.au"), ("SoftBank", "softbank.jp"),
        ("SK Telecom", "sktelecom.com"),
    ])

    return root


# ---------- Driver ------------------------------------------------------------


@dataclass
class Stats:
    groups_created: int = 0
    groups_skipped: int = 0
    hosts_created: int = 0
    hosts_skipped: int = 0
    hosts_failed: int = 0
    by_probe: dict = field(default_factory=dict)

    def bump_host(self, probe: str, key: str) -> None:
        d = self.by_probe.setdefault(probe, {"created": 0, "skipped": 0, "failed": 0})
        d[key] += 1


def ensure_groups(client: HazeClient, root: GroupNode, stats: Stats) -> dict[int, str]:
    """DFS through root's children, creating each group. Returns id(node) -> uuid."""
    existing = client.list_groups() if not client.dry_run else []
    index: dict[tuple[str, str], str] = {
        ((g.get("parent_uuid") or ""), g["display_name"]): g["uuid"] for g in existing
    }

    uuid_map: dict[int, str] = {}

    def walk(node: GroupNode, parent_uuid: str | None) -> None:
        for child in node.children:
            key = ((parent_uuid or ""), child.name)
            uuid = index.get(key)
            if uuid is None:
                status, body = client.create_group(child.name, parent_uuid)
                if status == 201:
                    uuid = body["uuid"]
                    stats.groups_created += 1
                elif status == 409:
                    # Race: refresh and retry the lookup
                    fresh = client.list_groups()
                    for g in fresh:
                        if (g.get("parent_uuid") or "") == (parent_uuid or "") \
                                and g["display_name"] == child.name:
                            uuid = g["uuid"]
                            break
                    if uuid is None:
                        print(f"  ! conflict but no match for {child.name}", file=sys.stderr)
                        continue
                    stats.groups_skipped += 1
                else:
                    print(f"  ! group {child.name!r}: {status} {body}", file=sys.stderr)
                    continue
                index[key] = uuid
            else:
                stats.groups_skipped += 1
            uuid_map[id(child)] = uuid
            walk(child, uuid)

    walk(root, None)
    return uuid_map


def ensure_hosts(client: HazeClient, root: GroupNode, uuid_map: dict[int, str],
                 interval: int, samples: int, stats: Stats) -> None:
    existing_names = set()
    if not client.dry_run:
        existing_names = {h["display_name"] for h in client.list_hosts()}

    def walk(node: GroupNode) -> None:
        group_uuid = uuid_map.get(id(node))
        for h in node.hosts:
            if h.display_name in existing_names:
                stats.hosts_skipped += 1
                stats.bump_host(h.probe_type, "skipped")
                continue
            body = {
                "group_uuids": [group_uuid] if group_uuid else [],
                "display_name": h.display_name,
                "probe_type": h.probe_type,
                "probe_config": h.probe_config,
                "interval_secs": interval,
                "samples_per_period": samples,
            }
            status, resp = client.create_host(body)
            if status == 201:
                stats.hosts_created += 1
                stats.bump_host(h.probe_type, "created")
                existing_names.add(h.display_name)
            elif status == 409:
                stats.hosts_skipped += 1
                stats.bump_host(h.probe_type, "skipped")
            else:
                stats.hosts_failed += 1
                stats.bump_host(h.probe_type, "failed")
                print(f"  ! host {h.display_name!r} ({h.probe_type}): {status} {resp}",
                      file=sys.stderr)
        for child in node.children:
            walk(child)

    walk(root)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--url", required=True, help="Haze base URL (e.g. http://localhost:5173)")
    parser.add_argument("--token", required=True, help="API token (hzt_...)")
    parser.add_argument("--interval-secs", type=int, default=5,
                        help="Default probe interval in seconds (default: 5)")
    parser.add_argument("--samples-per-period", type=int, default=10,
                        help="Default samples per period (default: 10)")
    parser.add_argument("--dry-run", action="store_true",
                        help="Print what would be created without POSTing")
    args = parser.parse_args(argv)

    if args.interval_secs < 1:
        parser.error("--interval-secs must be >= 1")
    if not 1 <= args.samples_per_period <= 1000:
        parser.error("--samples-per-period must be 1..1000")

    client = HazeClient(args.url, args.token, dry_run=args.dry_run)
    tree = build_tree()

    stats = Stats()
    print(f"Seeding Haze at {args.url} "
          f"(interval={args.interval_secs}s, samples={args.samples_per_period}, "
          f"dry_run={args.dry_run})")

    uuid_map = ensure_groups(client, tree, stats)
    ensure_hosts(client, tree, uuid_map, args.interval_secs, args.samples_per_period, stats)

    print("\n--- summary ---")
    print(f"groups: created={stats.groups_created} skipped={stats.groups_skipped}")
    print(f"hosts:  created={stats.hosts_created} skipped={stats.hosts_skipped} "
          f"failed={stats.hosts_failed}")
    for probe, counts in sorted(stats.by_probe.items()):
        print(f"  {probe:14s} created={counts['created']:4d} "
              f"skipped={counts['skipped']:4d} failed={counts['failed']:4d}")
    return 0 if stats.hosts_failed == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
