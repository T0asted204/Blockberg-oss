"""Blockberg agent: runs inside Minecraft (Minescript 5.0) on the laptop.

Install: copy to <instance>/minecraft/minescript/ and type `\\blockberg_agent` in chat.
Listens on TCP :7777 for one JSON command per line from the Rust terminal.

Commands -> replies
    instabuy / instasell   {"cmd","product","qty"}          -> {"type":"fill","msg","cost"?,"signals"}
    buyorder / selloffer   {"cmd","product","qty","price"}  -> {"type":"placed","msg","order"?}
    orders                                                 -> {"type":"orders","orders":[{side,product,amount,price,filled,total,claimable}]}
    claimorder             {"product","side","price"}       -> {"type":"claimed","msg","signals",...}
    cancel / claim                                         -> {"type":"log"} (+ balance)
    balance                {"bank":true?}                   -> {"type":"balance","purse","bank"}
    invall                                                 -> {"type":"invall","items":[{id,count,fits}],"free_slots","slots":[{slot,id,count}]}
    requote                {"product","price","slice","remaining","max_qty","max_price"} -> {"type":"requoted",...}  (cancel + re-place in one go)
    sackscan                                               -> {"type":"sacks","sacks":[{name,copies,cap_each,items:[{name,stored}]}]}  (opens every sack once)
    run/open/click/sign/close/screen/dump/inv              -> debugging helpers
and pushes {"type":"bazaar","msg"} for every "[Bazaar] ..." chat line.

Fill verification ("signals"): chat (Hypixel's own message), orders (Manage Orders menu state),
inventory (item count change), purse (coin change). A fill is *verified* when
chat AND (orders OR inventory OR purse). The terminal does that logic.

Verified in-game (Minecraft 26.1.2, Minescript 5.0): clicks via MultiPlayerGameMode.handleContainerInput,
signs via SignEditScreen.charTyped, chat via logs/latest.log, purse via scoreboard, bank via /bank.
Every action closes all open menus/GUIs before and after so chat lines are never obscured.
Hypixel's "selling far below average price" confirm screen is never confirmed automatically.
"""
import contextlib
import os
import json
import re
import socket
import sys
import threading
import random
import time

import minescript as ms
from java import JavaClass, null

def _port(env, key, default):
    """A second agent on the same machine has its own ports: from the environment, else from bb_ports.json next to this file
    (Minescript starts the script with a cleaned environment, so the file is what really works)."""
    if os.environ.get(env):
        return int(os.environ[env])
    try:
        with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "bb_ports.json")) as f:
            return int(json.load(f)[key])
    except Exception:
        return default


PORT = _port("BB_AGENT_PORT", "agent", 7777)
AGENT_VERSION = "2026-11-18.1"   # bump when deploying; the app shows it and can ask for a restart
STACK = 64
MAX_UNITS = 71680  # Hypixel's per-order limit (more than the inventory holds is fine when sacks have room)
BANK_TTL = 300  # seconds a cached bank balance is considered fresh

Minecraft = JavaClass("net.minecraft.client.Minecraft")
ContainerInput = JavaClass("net.minecraft.world.inventory.ContainerInput")
CharacterEvent = JavaClass("net.minecraft.client.input.CharacterEvent")
_last_menu_act = [0.0]   # when we last clicked in / closed a menu
MENU_GAP = 0.3           # Hypixel drops the first click of a menu opened sooner than this after the previous menu activity


def menu_gap():
    """Wait out the rest of MENU_GAP since the last menu click/close, so the next menu is opened when clicks will count."""
    wait = MENU_GAP - (time.time() - _last_menu_act[0])
    if wait > 0:
        time.sleep(wait)


_PICKUP = ContainerInput.PICKUP   # a constant: looking it up on every click cost ~0.1 s each time
_jv = {"mc": None, "gm": None, "pl": None, "t": 0.0}


def _java_refs():
    """Minecraft / game mode / player handles, re-read every 10 s (each Java access costs ~0.1 s, so not on every click)."""
    now = time.time()
    if _jv["mc"] is None or now - _jv["t"] > 10:
        mc = Minecraft.getInstance()
        _jv["mc"], _jv["pl"], _jv["gm"], _jv["t"] = mc, mc.player, mc.gameMode, now
    return _jv["mc"], _jv["pl"], _jv["gm"]

MIN_CLICK_GAP = 0.05          # shortest pause after a click (Hypixel can ignore clicks that arrive faster)
CLICK_TIMEOUT = 1.2           # how long to wait for a menu to change after a click
_tl = threading.local()       # per-command timing marks
_say_at = [0.0]               # time of the last chat message we sent (rate limit)
_chat_subs = set()            # connections that asked for the live chat feed (opt-in, so old apps never see it)
_wlock = {}                   # connection -> write lock (late "signals" messages come from another thread)
_lock = threading.Lock()      # one in-game action at a time


def begin_timing():
    _tl.marks, _tl.t0 = [], time.time()


def tick(label):
    marks = getattr(_tl, "marks", None)
    if marks is not None:
        marks.append((label, round(time.time() - _tl.t0, 2)))


def timing():
    if not hasattr(_tl, "t0"):
        return {}
    return {"total_s": round(time.time() - _tl.t0, 2), "marks": _tl.marks[-40:]}
_clients = []                 # connected terminals, for pushed chat events
_chat = []                    # (timestamp, text) of recent [Bazaar] chat lines
_syslog = []                  # (timestamp, text) of recent chat lines of any kind (stash messages and the like)
_sacklog = []                 # (timestamp, signed item count) from "[Sacks] +14 items. (Last 5s.)" lines
SACK_VERIFY = "chat"          # "chat": verify sack movement from [Sacks] chat lines (fast); "menu": read the sack menus (slow)
_bank = {"value": None, "at": 0.0}


def strip_fmt(s: str) -> str:
    return re.sub(r"§.", "", s or "")


def num(s) -> float:
    return float(str(s).replace(",", ""))


# ---------- reading the game ----------

def purse():
    sb = ms.get_scoreboard()
    if not sb:
        return None
    for e in sb.entries:
        text = strip_fmt(e.display_name) + " " + strip_fmt(e.name)
        m = re.search(r"(?:Purse|Piggy):\s*([\d,\.]+)", text)
        if m:
            return num(m.group(1))
    return None


def item_name(it) -> str:
    """Display name of an item. Hypixel splits names into coloured fragments, so join them."""
    raw = strip_fmt(str(it.nbt))
    head = raw.split('"minecraft:lore"')[0]
    parts = re.findall(r'text:"([^"]*)"', head)
    return "".join(parts) if parts else raw


def item_lore(it) -> str:
    raw = strip_fmt(str(it.nbt))
    if '"minecraft:lore"' not in raw:
        return ""
    tail = raw.split('"minecraft:lore"', 1)[1]
    return " ".join(re.findall(r'text:"([^"]*)"', tail)).strip()


def screen_class() -> str:
    """Class name of the open screen, "none" when nothing is open. Every Java call through the bridge costs ~0.1 s, so the
    common cases are answered from Minescript's own fast calls (~0.02 s) and the Java lookup is only the fallback."""
    try:
        n = ms.screen_name()
    except Exception:
        n = "?"
    if n is None:
        return "none"
    if n == "Edit Sign Message":
        return "net.minecraft.client.gui.screens.inventory.SignEditScreen"
    if n != "?" and ms.container_get_items():
        return "net.minecraft.client.gui.screens.inventory.ContainerScreen"
    scr = Minecraft.getInstance().screen
    return str(scr.getClass().getName()) if scr else "none"


def menu_open() -> bool:
    return "ContainerScreen" in screen_class()


def dump_items(lore=False):
    out = []
    for it in ms.container_get_items() or []:
        n = item_name(it)
        if n and it.item != "minecraft:air":
            d = {"slot": it.slot, "item": it.item, "name": n[:120]}
            if lore:
                d["lore"] = item_lore(it)[:300]
            out.append(d)
    return out


_msig = [None, 0.0]   # fingerprint of the last menu content we looked at, and when it first appeared
SIGN_DELAY = 0.2
MENU_CLICK_DELAY = 0.22   # Hypixel ignores a click on a menu that appeared less than ~0.15 s ago


def _note_menu(items):
    """Remember when the current menu content first appeared (so a click can wait until it will count)."""
    if not items:
        _msig[0] = None
        return None
    sig = hash(tuple((it.slot, it.item, it.count, str(it.nbt)) for it in items))
    if sig != _msig[0]:
        _msig[0], _msig[1] = sig, time.time()
    return sig


def slot_with(text: str, exact=False):
    items = ms.container_get_items() or []
    _note_menu(items)
    for it in items:
        n = item_name(it).lower()
        if (n == text.lower()) if exact else (text.lower() in n):
            return it.slot
    return None


def wait_for(text: str, exact=False, timeout=3.0):
    end = time.time() + timeout
    while time.time() < end:
        s = slot_with(text, exact)
        if s is not None:
            return s
        time.sleep(0.04)
    raise RuntimeError(f"expected '{text}' in the open menu but it never appeared")


def inventory_items():
    return ms.player_inventory() or []


# The Personal Compactor silently merges N of an item into 1 of the next tier, so a single item's count can
# drop without any trade. Counting the whole upgrade tree in raw units is immune to that.
# --- generated from the NotEnoughUpdates item repo (exact crafting recipes), see tools note ---
# RELATIONS: upgraded item -> (ingredient, how many ingredients per 1 upgraded item). 216 single-ingredient recipes;
# ratios are NOT all 160 (44 are 80, 11 are 9, plus 144/192/576/20/5), so never assume 160.
RELATIONS = {
    "ABSOLUTE_ENDER_PEARL": ("ENCHANTED_ENDER_PEARL", 80),
    "ACACIA_BIRDHOUSE": ("ENCHANTED_ACACIA_LOG", 384),
    "ALL_IN_ALOE": ("ALL_IN_ALOE_FRAGMENT", 9),
    "BERBERIS_FUEL_INJECTOR": ("WILTED_BERBERIS", 336),
    "BLUE_ICE_HUNK": ("ICE_HUNK", 160),
    "BOX_OF_SEEDS": ("ENCHANTED_SEEDS", 160),
    "COMPACTED_MOONFLOWER": ("ENCHANTED_MOONFLOWER", 160),
    "COMPACTED_SUNFLOWER": ("ENCHANTED_SUNFLOWER", 160),
    "COMPACTED_WILD_ROSE": ("ENCHANTED_WILD_ROSE", 160),
    "CONCENTRATED_STONE": ("ENCHANTED_HARD_STONE", 576),
    "CONDENSED_FERMENTO": ("FERMENTO", 9),
    "CONDENSED_HELIANTHUS": ("HELIANTHUS", 9),
    "CONDENSED_WATER_LILY": ("ENCHANTED_WATER_LILY", 160),
    "ENCHANTED_ACACIA_LOG": ("LOG_2", 160),
    "ENCHANTED_ANCIENT_CLAW": ("ANCIENT_CLAW", 160),
    "ENCHANTED_BAKED_POTATO": ("ENCHANTED_POTATO", 160),
    "ENCHANTED_BIRCH_LOG": ("LOG-2", 160),
    "ENCHANTED_BLAZE_POWDER": ("BLAZE_ROD", 160),
    "ENCHANTED_BLAZE_ROD": ("ENCHANTED_BLAZE_POWDER", 160),
    "ENCHANTED_BONE": ("BONE", 160),
    "ENCHANTED_BONE_BLOCK": ("ENCHANTED_BONE", 160),
    "ENCHANTED_BREAD": ("WHEAT", 60),
    "ENCHANTED_BROWN_MUSHROOM": ("BROWN_MUSHROOM", 160),
    "ENCHANTED_CACTUS": ("ENCHANTED_CACTUS_GREEN", 160),
    "ENCHANTED_CACTUS_GREEN": ("CACTUS", 160),
    "ENCHANTED_CARROT": ("CARROT_ITEM", 160),
    "ENCHANTED_CLAY_BALL": ("CLAY_BALL", 160),
    "ENCHANTED_CLAY_BLOCK": ("ENCHANTED_CLAY_BALL", 160),
    "ENCHANTED_CLOWNFISH": ("RAW_FISH-2", 160),
    "ENCHANTED_COAL": ("COAL", 160),
    "ENCHANTED_COAL_BLOCK": ("ENCHANTED_COAL", 160),
    "ENCHANTED_COBBLESTONE": ("COBBLESTONE", 160),
    "ENCHANTED_COCOA": ("INK_SACK-3", 160),
    "ENCHANTED_COMPOST": ("COMPOST", 160),
    "ENCHANTED_COOKED_FISH": ("ENCHANTED_RAW_FISH", 160),
    "ENCHANTED_COOKED_MUTTON": ("ENCHANTED_MUTTON", 160),
    "ENCHANTED_COOKED_RABBIT": ("ENCHANTED_RABBIT", 160),
    "ENCHANTED_COOKED_SALMON": ("ENCHANTED_RAW_SALMON", 160),
    "ENCHANTED_COOKIE": ("ENCHANTED_COCOA", 160),
    "ENCHANTED_DANDELION": ("YELLOW_FLOWER", 160),
    "ENCHANTED_DARK_OAK_LOG": ("LOG_2-1", 160),
    "ENCHANTED_DIAMOND": ("DIAMOND", 160),
    "ENCHANTED_DIAMOND_BLOCK": ("ENCHANTED_DIAMOND", 160),
    "ENCHANTED_EGG": ("EGG", 144),
    "ENCHANTED_EMERALD": ("EMERALD", 160),
    "ENCHANTED_EMERALD_BLOCK": ("ENCHANTED_EMERALD", 160),
    "ENCHANTED_ENDER_PEARL": ("ENDER_PEARL", 20),
    "ENCHANTED_ENDSTONE": ("ENDER_STONE", 160),
    "ENCHANTED_FEATHER": ("FEATHER", 160),
    "ENCHANTED_FIG_LOG": ("FIG_LOG", 160),
    "ENCHANTED_FLINT": ("FLINT", 160),
    "ENCHANTED_GHAST_TEAR": ("GHAST_TEAR", 5),
    "ENCHANTED_GLACITE": ("GLACITE", 160),
    "ENCHANTED_GLISTERING_MELON": ("SPECKLED_MELON", 256),
    "ENCHANTED_GLOWSTONE": ("ENCHANTED_GLOWSTONE_DUST", 160),
    "ENCHANTED_GLOWSTONE_DUST": ("GLOWSTONE_DUST", 160),
    "ENCHANTED_GOLD": ("GOLD_INGOT", 160),
    "ENCHANTED_GOLDEN_CARROT": ("ENCHANTED_CARROT", 160),
    "ENCHANTED_GOLD_BLOCK": ("ENCHANTED_GOLD", 160),
    "ENCHANTED_GRILLED_PORK": ("ENCHANTED_PORK", 160),
    "ENCHANTED_GUNPOWDER": ("SULPHUR", 160),
    "ENCHANTED_HARD_STONE": ("HARD_STONE", 576),
    "ENCHANTED_HAY_BALE": ("ENCHANTED_WHEAT", 160),
    "ENCHANTED_HELIX_LOG": ("HELIX_LOG", 160),
    "ENCHANTED_HONEYCOMB": ("HONEYCOMB", 160),
    "ENCHANTED_HONEYCOMB_BLOCK": ("ENCHANTED_HONEYCOMB", 160),
    "ENCHANTED_HUGE_MUSHROOM_1": ("ENCHANTED_BROWN_MUSHROOM", 160),
    "ENCHANTED_HUGE_MUSHROOM_2": ("ENCHANTED_RED_MUSHROOM", 160),
    "ENCHANTED_ICE": ("ICE", 160),
    "ENCHANTED_INK_SACK": ("INK_SACK", 80),
    "ENCHANTED_IRON": ("IRON_INGOT", 160),
    "ENCHANTED_IRON_BLOCK": ("ENCHANTED_IRON", 160),
    "ENCHANTED_JUNGLE_LOG": ("LOG-3", 160),
    "ENCHANTED_LAPIS_LAZULI": ("INK_SACK-4", 160),
    "ENCHANTED_LAPIS_LAZULI_BLOCK": ("ENCHANTED_LAPIS_LAZULI", 160),
    "ENCHANTED_LEATHER": ("LEATHER", 160),
    "ENCHANTED_LUSH_BERBERIS": ("LUSH_BERBERIS", 160),
    "ENCHANTED_MAGMA_CREAM": ("MAGMA_CREAM", 160),
    "ENCHANTED_MANGROVE_LOG": ("MANGROVE_LOG", 160),
    "ENCHANTED_MELON": ("MELON", 160),
    "ENCHANTED_MELON_BLOCK": ("ENCHANTED_MELON", 160),
    "ENCHANTED_MITHRIL": ("MITHRIL_ORE", 160),
    "ENCHANTED_MOONFLOWER": ("MOONFLOWER", 160),
    "ENCHANTED_MUTTON": ("MUTTON", 160),
    "ENCHANTED_MYCELIUM": ("MYCEL", 160),
    "ENCHANTED_MYCELIUM_CUBE": ("ENCHANTED_MYCELIUM", 160),
    "ENCHANTED_NETHERRACK": ("NETHERRACK", 160),
    "ENCHANTED_NETHER_STALK": ("NETHER_STALK", 160),
    "ENCHANTED_OAK_LOG": ("LOG", 160),
    "ENCHANTED_OBSIDIAN": ("OBSIDIAN", 160),
    "ENCHANTED_PACKED_ICE": ("ENCHANTED_ICE", 160),
    "ENCHANTED_PAPER": ("SUGAR_CANE", 192),
    "ENCHANTED_POISONOUS_POTATO": ("POISONOUS_POTATO", 160),
    "ENCHANTED_POPPY": ("RED_ROSE", 576),
    "ENCHANTED_PORK": ("PORK", 160),
    "ENCHANTED_POTATO": ("POTATO_ITEM", 160),
    "ENCHANTED_PRISMARINE_CRYSTALS": ("PRISMARINE_CRYSTALS", 80),
    "ENCHANTED_PRISMARINE_SHARD": ("PRISMARINE_SHARD", 80),
    "ENCHANTED_PUFFERFISH": ("RAW_FISH-3", 160),
    "ENCHANTED_PUMPKIN": ("PUMPKIN", 160),
    "ENCHANTED_QUARTZ": ("QUARTZ", 160),
    "ENCHANTED_QUARTZ_BLOCK": ("ENCHANTED_QUARTZ", 160),
    "ENCHANTED_RABBIT": ("RABBIT", 160),
    "ENCHANTED_RABBIT_FOOT": ("RABBIT_FOOT", 160),
    "ENCHANTED_RABBIT_HIDE": ("RABBIT_HIDE", 160),
    "ENCHANTED_RAW_BEEF": ("RAW_BEEF", 160),
    "ENCHANTED_RAW_CHICKEN": ("RAW_CHICKEN", 160),
    "ENCHANTED_RAW_FISH": ("RAW_FISH", 160),
    "ENCHANTED_RAW_SALMON": ("RAW_FISH-1", 160),
    "ENCHANTED_REDSTONE": ("REDSTONE", 160),
    "ENCHANTED_REDSTONE_BLOCK": ("ENCHANTED_REDSTONE", 160),
    "ENCHANTED_RED_MUSHROOM": ("RED_MUSHROOM", 160),
    "ENCHANTED_RED_SAND": ("SAND-1", 160),
    "ENCHANTED_RED_SAND_CUBE": ("ENCHANTED_RED_SAND", 160),
    "ENCHANTED_ROTTEN_FLESH": ("ROTTEN_FLESH", 160),
    "ENCHANTED_RUBY_VEILSHROOM": ("RUBY_VEILSHROOM", 160),
    "ENCHANTED_SAND": ("SAND", 160),
    "ENCHANTED_SEA_LUMIES": ("SEA_LUMIES", 160),
    "ENCHANTED_SEEDS": ("SEEDS", 160),
    "ENCHANTED_SHARK_FIN": ("SHARK_FIN", 160),
    "ENCHANTED_SLIME_BALL": ("SLIME_BALL", 160),
    "ENCHANTED_SLIME_BLOCK": ("ENCHANTED_SLIME_BALL", 160),
    "ENCHANTED_SNOW_BLOCK": ("SNOW_BLOCK", 160),
    "ENCHANTED_SPIDER_EYE": ("SPIDER_EYE", 160),
    "ENCHANTED_SPONGE": ("SPONGE", 40),
    "ENCHANTED_SPRUCE_LOG": ("LOG-1", 160),
    "ENCHANTED_STRING": ("STRING", 160),
    "ENCHANTED_SUGAR": ("SUGAR_CANE", 160),
    "ENCHANTED_SUGAR_CANE": ("ENCHANTED_SUGAR", 160),
    "ENCHANTED_SULPHUR": ("SULPHUR_ORE", 160),
    "ENCHANTED_SULPHUR_CUBE": ("ENCHANTED_SULPHUR", 160),
    "ENCHANTED_SUNFLOWER": ("DOUBLE_PLANT", 160),
    "ENCHANTED_TENDER_WOOD": ("TENDER_WOOD", 160),
    "ENCHANTED_TITANIUM": ("TITANIUM_ORE", 160),
    "ENCHANTED_TUNGSTEN": ("TUNGSTEN", 160),
    "ENCHANTED_TURBO_GOURD": ("TURBO_GOURD", 5),
    "ENCHANTED_UMBER": ("UMBER", 160),
    "ENCHANTED_VINESAP": ("VINESAP", 160),
    "ENCHANTED_WATER_LILY": ("WATER_LILY", 160),
    "ENCHANTED_WET_SPONGE": ("ENCHANTED_SPONGE", 40),
    "ENCHANTED_WHEAT": ("WHEAT", 160),
    "ENCHANTED_WILD_ROSE": ("WILD_ROSE", 160),
    "ENCHANTED_WOOL": ("WOOL", 160),
    "FIGSTONE": ("ENCHANTED_FIG_LOG", 160),
    "FINE_AMBER_GEM": ("FLAWED_AMBER_GEM", 80),
    "FINE_AMETHYST_GEM": ("FLAWED_AMETHYST_GEM", 80),
    "FINE_AQUAMARINE_GEM": ("FLAWED_AQUAMARINE_GEM", 80),
    "FINE_CITRINE_GEM": ("FLAWED_CITRINE_GEM", 80),
    "FINE_JADE_GEM": ("FLAWED_JADE_GEM", 80),
    "FINE_JASPER_GEM": ("FLAWED_JASPER_GEM", 80),
    "FINE_ONYX_GEM": ("FLAWED_ONYX_GEM", 80),
    "FINE_OPAL_GEM": ("FLAWED_OPAL_GEM", 80),
    "FINE_PERIDOT_GEM": ("FLAWED_PERIDOT_GEM", 80),
    "FINE_RUBY_GEM": ("FLAWED_RUBY_GEM", 80),
    "FINE_SAPPHIRE_GEM": ("FLAWED_SAPPHIRE_GEM", 80),
    "FINE_TOPAZ_GEM": ("FLAWED_TOPAZ_GEM", 80),
    "FLAWED_AMBER_GEM": ("ROUGH_AMBER_GEM", 80),
    "FLAWED_AMETHYST_GEM": ("ROUGH_AMETHYST_GEM", 80),
    "FLAWED_AQUAMARINE_GEM": ("ROUGH_AQUAMARINE_GEM", 80),
    "FLAWED_CITRINE_GEM": ("ROUGH_CITRINE_GEM", 80),
    "FLAWED_JADE_GEM": ("ROUGH_JADE_GEM", 80),
    "FLAWED_JASPER_GEM": ("ROUGH_JASPER_GEM", 80),
    "FLAWED_ONYX_GEM": ("ROUGH_ONYX_GEM", 80),
    "FLAWED_OPAL_GEM": ("ROUGH_OPAL_GEM", 80),
    "FLAWED_PERIDOT_GEM": ("ROUGH_PERIDOT_GEM", 80),
    "FLAWED_RUBY_GEM": ("ROUGH_RUBY_GEM", 80),
    "FLAWED_SAPPHIRE_GEM": ("ROUGH_SAPPHIRE_GEM", 80),
    "FLAWED_TOPAZ_GEM": ("ROUGH_TOPAZ_GEM", 80),
    "FLAWLESS_AMBER_GEM": ("FINE_AMBER_GEM", 80),
    "FLAWLESS_AMETHYST_GEM": ("FINE_AMETHYST_GEM", 80),
    "FLAWLESS_AQUAMARINE_GEM": ("FINE_AQUAMARINE_GEM", 80),
    "FLAWLESS_CITRINE_GEM": ("FINE_CITRINE_GEM", 80),
    "FLAWLESS_JADE_GEM": ("FINE_JADE_GEM", 80),
    "FLAWLESS_JASPER_GEM": ("FINE_JASPER_GEM", 80),
    "FLAWLESS_ONYX_GEM": ("FINE_ONYX_GEM", 80),
    "FLAWLESS_OPAL_GEM": ("FINE_OPAL_GEM", 80),
    "FLAWLESS_PERIDOT_GEM": ("FINE_PERIDOT_GEM", 80),
    "FLAWLESS_RUBY_GEM": ("FINE_RUBY_GEM", 80),
    "FLAWLESS_SAPPHIRE_GEM": ("FINE_SAPPHIRE_GEM", 80),
    "FLAWLESS_TOPAZ_GEM": ("FINE_TOPAZ_GEM", 80),
    "HAY_BLOCK": ("WHEAT", 9),
    "HELIXIS": ("ENCHANTED_HELIX_LOG", 160),
    "HEMOBOMB": ("HEMOGLASS", 15),
    "HEMOGLASS": ("HEMOVIBE", 160),
    "HUGE_MUSHROOM_1": ("BROWN_MUSHROOM", 9),
    "HUGE_MUSHROOM_2": ("RED_MUSHROOM", 9),
    "LEATHER": ("RABBIT_HIDE", 4),
    "LOTUS_DIAMOND": ("LOTUS_GOLD", 20),
    "LOTUS_GOLD": ("LOTUS_SILVER", 80),
    "LOTUS_SILVER": ("LOTUS", 80),
    "MAGMA_FISH_DIAMOND": ("MAGMA_FISH_GOLD", 20),
    "MAGMA_FISH_GOLD": ("MAGMA_FISH_SILVER", 80),
    "MAGMA_FISH_SILVER": ("MAGMA_FISH", 80),
    "MANGCORE": ("ENCHANTED_MANGROVE_LOG", 160),
    "MANGROVE_GEM": ("ENCHANTED_MANGROVE_LOG", 144),
    "MELON_BLOCK": ("MELON", 9),
    "MINNOW_BAIT": ("RAW_FISH", 2),
    "MIXED_MITE_GEL": ("MITE_GEL", 160),
    "MUTANT_NETHER_STALK": ("ENCHANTED_NETHER_STALK", 160),
    "OMEGA_EGG": ("SUPER_EGG", 9),
    "PACKED_ICE": ("ICE", 9),
    "POLISHED_PUMPKIN": ("ENCHANTED_PUMPKIN", 160),
    "PRICKLY_KISS": ("ENCHANTED_WILD_ROSE", 5),
    "SHARK_BAIT": ("SHARK_FIN", 2),
    "SILVER_FANG": ("ENCHANTED_GHAST_TEAR", 25),
    "SIL_EX": ("CONCENTRATED_STONE", 9),
    "SIMPLE_CARROT_CANDY": ("CARROT_ITEM", 576),
    "SMALL_ENCHANTED_CHEST": ("LOG", 64),
    "SNOW_BLOCK": ("SNOW_BALL", 4),
    "SOULFLOW": ("RAW_SOULFLOW", 160),
    "STUFFED_CHILI_PEPPER": ("CHILI_PEPPER", 160),
    "SUNFLOWER_OIL": ("ENCHANTED_SUNFLOWER", 9),
    "SUPER_EGG": ("ENCHANTED_EGG", 144),
    "VEILSHROOM_BUNCH": ("ENCHANTED_RUBY_VEILSHROOM", 160),
    "VERY_CRUDE_GABAGOOL": ("CRUDE_GABAGOOL", 192),
    "WHIPPED_MAGMA_CREAM": ("ENCHANTED_MAGMA_CREAM", 160),
}
# NAMES: SkyBlock id -> in-game display name (what bazaar pages and sacks show)
NAMES = {
    "ABSOLUTE_ENDER_PEARL": "Absolute Ender Pearl",
    "ACACIA_BIRDHOUSE": "Acacia Birdhouse",
    "ALL_IN_ALOE": "All-in Aloe",
    "ALL_IN_ALOE_FRAGMENT": "All-in Aloe Fragment",
    "ANCIENT_CLAW": "Ancient Claw",
    "BERBERIS_FUEL_INJECTOR": "Berberis Fuel Injector",
    "BLAZE_ROD": "Blaze Rod",
    "BLUE_ICE_HUNK": "Hunk of Blue Ice",
    "BONE": "Bone",
    "BOX_OF_SEEDS": "Box of Seeds",
    "BROWN_MUSHROOM": "Brown Mushroom",
    "CACTUS": "Cactus",
    "CARROT_ITEM": "Carrot",
    "CHILI_PEPPER": "Chili Pepper",
    "CLAY_BALL": "Clay Ball",
    "COAL": "Coal",
    "COBBLESTONE": "Cobblestone",
    "COMPACTED_MOONFLOWER": "Compacted Moonflower",
    "COMPACTED_SUNFLOWER": "Compacted Sunflower",
    "COMPACTED_WILD_ROSE": "Compacted Wild Rose",
    "COMPOST": "Compost",
    "CONCENTRATED_STONE": "Concentrated Stone",
    "CONDENSED_FERMENTO": "Condensed Fermento",
    "CONDENSED_HELIANTHUS": "Condensed Helianthus",
    "CONDENSED_WATER_LILY": "Condensed Lily Pad",
    "CRUDE_GABAGOOL": "Crude Gabagool",
    "DIAMOND": "Diamond",
    "DOUBLE_PLANT": "Sunflower",
    "EGG": "Egg",
    "EMERALD": "Emerald",
    "ENCHANTED_ACACIA_LOG": "Enchanted Acacia Log",
    "ENCHANTED_ANCIENT_CLAW": "Enchanted Ancient Claw",
    "ENCHANTED_BAKED_POTATO": "Enchanted Baked Potato",
    "ENCHANTED_BIRCH_LOG": "Enchanted Birch Log",
    "ENCHANTED_BLAZE_POWDER": "Enchanted Blaze Powder",
    "ENCHANTED_BLAZE_ROD": "Enchanted Blaze Rod",
    "ENCHANTED_BONE": "Enchanted Bone",
    "ENCHANTED_BONE_BLOCK": "Enchanted Bone Block",
    "ENCHANTED_BREAD": "Enchanted Bread",
    "ENCHANTED_BROWN_MUSHROOM": "Enchanted Brown Mushroom",
    "ENCHANTED_CACTUS": "Enchanted Cactus",
    "ENCHANTED_CACTUS_GREEN": "Enchanted Cactus Green",
    "ENCHANTED_CARROT": "Enchanted Carrot",
    "ENCHANTED_CLAY_BALL": "Enchanted Clay Ball",
    "ENCHANTED_CLAY_BLOCK": "Enchanted Clay Block",
    "ENCHANTED_CLOWNFISH": "Enchanted Tropical Fish",
    "ENCHANTED_COAL": "Enchanted Coal",
    "ENCHANTED_COAL_BLOCK": "Enchanted Coal Block",
    "ENCHANTED_COBBLESTONE": "Enchanted Cobblestone",
    "ENCHANTED_COCOA": "Enchanted Cocoa Beans",
    "ENCHANTED_COMPOST": "Compost Bundle",
    "ENCHANTED_COOKED_FISH": "Enchanted Cooked Cod",
    "ENCHANTED_COOKED_MUTTON": "Enchanted Cooked Mutton",
    "ENCHANTED_COOKED_RABBIT": "Enchanted Cooked Rabbit",
    "ENCHANTED_COOKED_SALMON": "Enchanted Cooked Salmon",
    "ENCHANTED_COOKIE": "Enchanted Cookie",
    "ENCHANTED_DANDELION": "Enchanted Dandelion",
    "ENCHANTED_DARK_OAK_LOG": "Enchanted Dark Oak Log",
    "ENCHANTED_DIAMOND": "Enchanted Diamond",
    "ENCHANTED_DIAMOND_BLOCK": "Enchanted Diamond Block",
    "ENCHANTED_EGG": "Enchanted Egg",
    "ENCHANTED_EMERALD": "Enchanted Emerald",
    "ENCHANTED_EMERALD_BLOCK": "Enchanted Emerald Block",
    "ENCHANTED_ENDER_PEARL": "Enchanted Ender Pearl",
    "ENCHANTED_ENDSTONE": "Enchanted End Stone",
    "ENCHANTED_FEATHER": "Enchanted Feather",
    "ENCHANTED_FIG_LOG": "Enchanted Fig Log",
    "ENCHANTED_FLINT": "Enchanted Flint",
    "ENCHANTED_GHAST_TEAR": "Enchanted Ghast Tear",
    "ENCHANTED_GLACITE": "Enchanted Glacite",
    "ENCHANTED_GLISTERING_MELON": "Enchanted Glistering Melon Slice",
    "ENCHANTED_GLOWSTONE": "Enchanted Glowstone",
    "ENCHANTED_GLOWSTONE_DUST": "Enchanted Glowstone Dust",
    "ENCHANTED_GOLD": "Enchanted Gold Ingot",
    "ENCHANTED_GOLDEN_CARROT": "Enchanted Golden Carrot",
    "ENCHANTED_GOLD_BLOCK": "Enchanted Gold Block",
    "ENCHANTED_GRILLED_PORK": "Enchanted Cooked Porkchop",
    "ENCHANTED_GUNPOWDER": "Enchanted Gunpowder",
    "ENCHANTED_HARD_STONE": "Enchanted Hard Stone",
    "ENCHANTED_HAY_BALE": "Enchanted Hay Bale",
    "ENCHANTED_HELIX_LOG": "Enchanted Helix Log",
    "ENCHANTED_HONEYCOMB": "Enchanted Honeycomb",
    "ENCHANTED_HONEYCOMB_BLOCK": "Enchanted Honeycomb Block",
    "ENCHANTED_HUGE_MUSHROOM_1": "Enchanted Brown Mushroom Block",
    "ENCHANTED_HUGE_MUSHROOM_2": "Enchanted Red Mushroom Block",
    "ENCHANTED_ICE": "Enchanted Ice",
    "ENCHANTED_INK_SACK": "Enchanted Ink Sac",
    "ENCHANTED_IRON": "Enchanted Iron Ingot",
    "ENCHANTED_IRON_BLOCK": "Enchanted Iron Block",
    "ENCHANTED_JUNGLE_LOG": "Enchanted Jungle Log",
    "ENCHANTED_LAPIS_LAZULI": "Enchanted Lapis Lazuli",
    "ENCHANTED_LAPIS_LAZULI_BLOCK": "Enchanted Lapis Lazuli Block",
    "ENCHANTED_LEATHER": "Enchanted Leather",
    "ENCHANTED_LUSH_BERBERIS": "Enchanted Lush Berberis",
    "ENCHANTED_MAGMA_CREAM": "Enchanted Magma Cream",
    "ENCHANTED_MANGROVE_LOG": "Enchanted Mangrove Log",
    "ENCHANTED_MELON": "Enchanted Melon Slice",
    "ENCHANTED_MELON_BLOCK": "Enchanted Melon",
    "ENCHANTED_MITHRIL": "Enchanted Mithril",
    "ENCHANTED_MOONFLOWER": "Enchanted Moonflower",
    "ENCHANTED_MUTTON": "Enchanted Raw Mutton",
    "ENCHANTED_MYCELIUM": "Enchanted Mycelium",
    "ENCHANTED_MYCELIUM_CUBE": "Enchanted Mycelium Cube",
    "ENCHANTED_NETHERRACK": "Enchanted Netherrack",
    "ENCHANTED_NETHER_STALK": "Enchanted Nether Wart",
    "ENCHANTED_OAK_LOG": "Enchanted Oak Log",
    "ENCHANTED_OBSIDIAN": "Enchanted Obsidian",
    "ENCHANTED_PACKED_ICE": "Enchanted Packed Ice",
    "ENCHANTED_PAPER": "Enchanted Paper",
    "ENCHANTED_POISONOUS_POTATO": "Enchanted Poisonous Potato",
    "ENCHANTED_POPPY": "Enchanted Poppy",
    "ENCHANTED_PORK": "Enchanted Raw Porkchop",
    "ENCHANTED_POTATO": "Enchanted Potato",
    "ENCHANTED_PRISMARINE_CRYSTALS": "Enchanted Prismarine Crystals",
    "ENCHANTED_PRISMARINE_SHARD": "Enchanted Prismarine Shard",
    "ENCHANTED_PUFFERFISH": "Enchanted Pufferfish",
    "ENCHANTED_PUMPKIN": "Enchanted Pumpkin",
    "ENCHANTED_QUARTZ": "Enchanted Nether Quartz",
    "ENCHANTED_QUARTZ_BLOCK": "Enchanted Quartz Block",
    "ENCHANTED_RABBIT": "Enchanted Raw Rabbit",
    "ENCHANTED_RABBIT_FOOT": "Enchanted Rabbit Foot",
    "ENCHANTED_RABBIT_HIDE": "Enchanted Rabbit Hide",
    "ENCHANTED_RAW_BEEF": "Enchanted Raw Beef",
    "ENCHANTED_RAW_CHICKEN": "Enchanted Raw Chicken",
    "ENCHANTED_RAW_FISH": "Enchanted Raw Cod",
    "ENCHANTED_RAW_SALMON": "Enchanted Raw Salmon",
    "ENCHANTED_REDSTONE": "Enchanted Redstone Dust",
    "ENCHANTED_REDSTONE_BLOCK": "Enchanted Redstone Block",
    "ENCHANTED_RED_MUSHROOM": "Enchanted Red Mushroom",
    "ENCHANTED_RED_SAND": "Enchanted Red Sand",
    "ENCHANTED_RED_SAND_CUBE": "Enchanted Red Sand Cube",
    "ENCHANTED_ROTTEN_FLESH": "Enchanted Rotten Flesh",
    "ENCHANTED_RUBY_VEILSHROOM": "Enchanted Ruby Veilshroom",
    "ENCHANTED_SAND": "Enchanted Sand",
    "ENCHANTED_SEA_LUMIES": "Enchanted Sea Lumies",
    "ENCHANTED_SEEDS": "Enchanted Seeds",
    "ENCHANTED_SHARK_FIN": "Enchanted Shark Fin",
    "ENCHANTED_SLIME_BALL": "Enchanted Slimeball",
    "ENCHANTED_SLIME_BLOCK": "Enchanted Slime Block",
    "ENCHANTED_SNOW_BLOCK": "Enchanted Snow Block",
    "ENCHANTED_SPIDER_EYE": "Enchanted Spider Eye",
    "ENCHANTED_SPONGE": "Enchanted Sponge",
    "ENCHANTED_SPRUCE_LOG": "Enchanted Spruce Log",
    "ENCHANTED_STRING": "Enchanted String",
    "ENCHANTED_SUGAR": "Enchanted Sugar",
    "ENCHANTED_SUGAR_CANE": "Enchanted Sugar Cane",
    "ENCHANTED_SULPHUR": "Enchanted Sulphur",
    "ENCHANTED_SULPHUR_CUBE": "Enchanted Sulphur Cube",
    "ENCHANTED_SUNFLOWER": "Enchanted Sunflower",
    "ENCHANTED_TENDER_WOOD": "Enchanted Tender Wood",
    "ENCHANTED_TITANIUM": "Enchanted Titanium",
    "ENCHANTED_TUNGSTEN": "Enchanted Tungsten",
    "ENCHANTED_TURBO_GOURD": "Enchanted Turbo Gourd",
    "ENCHANTED_UMBER": "Enchanted Umber",
    "ENCHANTED_VINESAP": "Enchanted Vinesap",
    "ENCHANTED_WATER_LILY": "Enchanted Lily Pad",
    "ENCHANTED_WET_SPONGE": "Enchanted Wet Sponge",
    "ENCHANTED_WHEAT": "Enchanted Wheat",
    "ENCHANTED_WILD_ROSE": "Enchanted Wild Rose",
    "ENCHANTED_WOOL": "Enchanted Wool",
    "ENDER_PEARL": "Ender Pearl",
    "ENDER_STONE": "End Stone",
    "FEATHER": "Feather",
    "FERMENTO": "Fermento",
    "FIGSTONE": "Figstone",
    "FIG_LOG": "Fig Log",
    "FINE_AMBER_GEM": "\ue015 Fine Amber Gemstone",
    "FINE_AMETHYST_GEM": "\ue008 Fine Amethyst Gemstone",
    "FINE_AQUAMARINE_GEM": "\ue00c Fine Aquamarine Gemstone",
    "FINE_CITRINE_GEM": "\ue054 Fine Citrine Gemstone",
    "FINE_JADE_GEM": "\ue053 Fine Jade Gemstone",
    "FINE_JASPER_GEM": "\ue00d Fine Jasper Gemstone",
    "FINE_ONYX_GEM": "\ue007 Fine Onyx Gemstone",
    "FINE_OPAL_GEM": "\ue027 Fine Opal Gemstone",
    "FINE_PERIDOT_GEM": "\ue051 Fine Peridot Gemstone",
    "FINE_RUBY_GEM": "\ue010 Fine Ruby Gemstone",
    "FINE_SAPPHIRE_GEM": "\ue003 Fine Sapphire Gemstone",
    "FINE_TOPAZ_GEM": "\ue01c Fine Topaz Gemstone",
    "FLAWED_AMBER_GEM": "\ue015 Flawed Amber Gemstone",
    "FLAWED_AMETHYST_GEM": "\ue008 Flawed Amethyst Gemstone",
    "FLAWED_AQUAMARINE_GEM": "\ue00c Flawed Aquamarine Gemstone",
    "FLAWED_CITRINE_GEM": "\ue054 Flawed Citrine Gemstone",
    "FLAWED_JADE_GEM": "\ue053 Flawed Jade Gemstone",
    "FLAWED_JASPER_GEM": "\ue00d Flawed Jasper Gemstone",
    "FLAWED_ONYX_GEM": "\ue007 Flawed Onyx Gemstone",
    "FLAWED_OPAL_GEM": "\ue027 Flawed Opal Gemstone",
    "FLAWED_PERIDOT_GEM": "\ue051 Flawed Peridot Gemstone",
    "FLAWED_RUBY_GEM": "\ue010 Flawed Ruby Gemstone",
    "FLAWED_SAPPHIRE_GEM": "\ue003 Flawed Sapphire Gemstone",
    "FLAWED_TOPAZ_GEM": "\ue01c Flawed Topaz Gemstone",
    "FLAWLESS_AMBER_GEM": "\ue015 Flawless Amber Gemstone",
    "FLAWLESS_AMETHYST_GEM": "\ue008 Flawless Amethyst Gemstone",
    "FLAWLESS_AQUAMARINE_GEM": "\ue00c Flawless Aquamarine Gemstone",
    "FLAWLESS_CITRINE_GEM": "\ue054 Flawless Citrine Gemstone",
    "FLAWLESS_JADE_GEM": "\ue053 Flawless Jade Gemstone",
    "FLAWLESS_JASPER_GEM": "\ue00d Flawless Jasper Gemstone",
    "FLAWLESS_ONYX_GEM": "\ue007 Flawless Onyx Gemstone",
    "FLAWLESS_OPAL_GEM": "\ue027 Flawless Opal Gemstone",
    "FLAWLESS_PERIDOT_GEM": "\ue051 Flawless Peridot Gemstone",
    "FLAWLESS_RUBY_GEM": "\ue010 Flawless Ruby Gemstone",
    "FLAWLESS_SAPPHIRE_GEM": "\ue003 Flawless Sapphire Gemstone",
    "FLAWLESS_TOPAZ_GEM": "\ue01c Flawless Topaz Gemstone",
    "FLINT": "Flint",
    "GHAST_TEAR": "Ghast Tear",
    "GLACITE": "Glacite",
    "GLOWSTONE_DUST": "Glowstone Dust",
    "GOLD_INGOT": "Gold Ingot",
    "HARD_STONE": "Hard Stone",
    "HAY_BLOCK": "Hay Bale",
    "HELIANTHUS": "Helianthus",
    "HELIXIS": "Helixis",
    "HELIX_LOG": "Helix Log",
    "HEMOBOMB": "Hemobomb",
    "HEMOGLASS": "Hemoglass",
    "HEMOVIBE": "Hemovibe",
    "HONEYCOMB": "Honeycomb",
    "HUGE_MUSHROOM_1": "Brown Mushroom Block",
    "HUGE_MUSHROOM_2": "Red Mushroom Block",
    "ICE": "Ice",
    "ICE_HUNK": "Hunk of Ice",
    "INK_SACK": "Ink Sac",
    "INK_SACK-3": "Cocoa Beans",
    "INK_SACK-4": "Lapis Lazuli",
    "IRON_INGOT": "Iron Ingot",
    "LEATHER": "Leather",
    "LOG": "Oak Log",
    "LOG-1": "Spruce Log",
    "LOG-2": "Birch Log",
    "LOG-3": "Jungle Log",
    "LOG_2": "Acacia Log",
    "LOG_2-1": "Dark Oak Log",
    "LOTUS": "Lotus",
    "LOTUS_DIAMOND": "Diamond Lotus",
    "LOTUS_GOLD": "Gold Lotus",
    "LOTUS_SILVER": "Silver Lotus",
    "LUSH_BERBERIS": "Lush Berberis",
    "MAGMA_CREAM": "Magma Cream",
    "MAGMA_FISH": "Magmafish",
    "MAGMA_FISH_DIAMOND": "Diamond Magmafish",
    "MAGMA_FISH_GOLD": "Gold Magmafish",
    "MAGMA_FISH_SILVER": "Silver Magmafish",
    "MANGCORE": "Mangcore",
    "MANGROVE_GEM": "Mangrove Gem",
    "MANGROVE_LOG": "Mangrove Log",
    "MELON": "Melon Slice",
    "MELON_BLOCK": "Melon",
    "MINNOW_BAIT": "Minnow Bait",
    "MITE_GEL": "Mite Gel",
    "MITHRIL_ORE": "Mithril",
    "MIXED_MITE_GEL": "Mixed Mite Gel",
    "MOONFLOWER": "Moonflower",
    "MUTANT_NETHER_STALK": "Mutant Nether Wart",
    "MUTTON": "Raw Mutton",
    "MYCEL": "Mycelium",
    "NETHERRACK": "Netherrack",
    "NETHER_STALK": "Nether Wart",
    "OBSIDIAN": "Obsidian",
    "OMEGA_EGG": "Omega Enchanted Egg",
    "PACKED_ICE": "Packed Ice",
    "POISONOUS_POTATO": "Poisonous Potato",
    "POLISHED_PUMPKIN": "Polished Pumpkin",
    "PORK": "Raw Porkchop",
    "POTATO_ITEM": "Potato",
    "PRICKLY_KISS": "Prickly Kiss",
    "PRISMARINE_CRYSTALS": "Prismarine Crystals",
    "PRISMARINE_SHARD": "Prismarine Shard",
    "PUMPKIN": "Pumpkin",
    "QUARTZ": "Nether Quartz",
    "RABBIT": "Raw Rabbit",
    "RABBIT_FOOT": "Rabbit's Foot",
    "RABBIT_HIDE": "Rabbit Hide",
    "RAW_BEEF": "Raw Beef",
    "RAW_CHICKEN": "Raw Chicken",
    "RAW_FISH": "Raw Cod",
    "RAW_FISH-1": "Raw Salmon",
    "RAW_FISH-2": "Tropical Fish",
    "RAW_FISH-3": "Pufferfish",
    "RAW_SOULFLOW": "Raw Soulflow",
    "REDSTONE": "Redstone Dust",
    "RED_MUSHROOM": "Red Mushroom",
    "RED_ROSE": "Poppy",
    "ROTTEN_FLESH": "Rotten Flesh",
    "ROUGH_AMBER_GEM": "\ue015 Rough Amber Gemstone",
    "ROUGH_AMETHYST_GEM": "\ue008 Rough Amethyst Gemstone",
    "ROUGH_AQUAMARINE_GEM": "\ue00c Rough Aquamarine Gemstone",
    "ROUGH_CITRINE_GEM": "\ue054 Rough Citrine Gemstone",
    "ROUGH_JADE_GEM": "\ue053 Rough Jade Gemstone",
    "ROUGH_JASPER_GEM": "\ue00d Rough Jasper Gemstone",
    "ROUGH_ONYX_GEM": "\ue007 Rough Onyx Gemstone",
    "ROUGH_OPAL_GEM": "\ue027 Rough Opal Gemstone",
    "ROUGH_PERIDOT_GEM": "\ue051 Rough Peridot Gemstone",
    "ROUGH_RUBY_GEM": "\ue010 Rough Ruby Gemstone",
    "ROUGH_SAPPHIRE_GEM": "\ue003 Rough Sapphire Gemstone",
    "ROUGH_TOPAZ_GEM": "\ue01c Rough Topaz Gemstone",
    "RUBY_VEILSHROOM": "Ruby Veilshroom",
    "SAND": "Sand",
    "SAND-1": "Red Sand",
    "SEA_LUMIES": "Sea Lumies",
    "SEEDS": "Seeds",
    "SHARK_BAIT": "Shark Bait",
    "SHARK_FIN": "Shark Fin",
    "SILVER_FANG": "Silver Fang",
    "SIL_EX": "Silex",
    "SIMPLE_CARROT_CANDY": "Simple Carrot Candy",
    "SLIME_BALL": "Slimeball",
    "SMALL_ENCHANTED_CHEST": "Small Storage",
    "SNOW_BALL": "Snowball",
    "SNOW_BLOCK": "Snow Block",
    "SOULFLOW": "Soulflow",
    "SPECKLED_MELON": "Glistering Melon Slice",
    "SPIDER_EYE": "Spider Eye",
    "SPONGE": "Sponge",
    "STRING": "String",
    "STUFFED_CHILI_PEPPER": "Stuffed Chili Pepper",
    "SUGAR_CANE": "Sugar Cane",
    "SULPHUR": "Gunpowder",
    "SULPHUR_ORE": "Sulphur",
    "SUNFLOWER_OIL": "Sunflower Oil",
    "SUPER_EGG": "Super Enchanted Egg",
    "TENDER_WOOD": "Tender Wood",
    "TITANIUM_ORE": "Titanium",
    "TUNGSTEN": "Tungsten",
    "TURBO_GOURD": "Turbo Gourd",
    "UMBER": "Umber",
    "VEILSHROOM_BUNCH": "Veilshroom Bunch",
    "VERY_CRUDE_GABAGOOL": "Very Crude Gabagool",
    "VINESAP": "Vinesap",
    "WATER_LILY": "Lily Pad",
    "WHEAT": "Wheat",
    "WHIPPED_MAGMA_CREAM": "Whipped Magma Cream",
    "WILD_ROSE": "Wild Rose",
    "WILTED_BERBERIS": "Wilted Berberis",
    "WOOL": "White Wool",
    "YELLOW_FLOWER": "Dandelion",
}
# ROOT_VANILLA: chain roots that are plain vanilla items (no custom id) -> vanilla item id.
ROOT_VANILLA = {
    "ALL_IN_ALOE_FRAGMENT": "minecraft:gold_nugget",
    "ANCIENT_CLAW": "minecraft:flint",
    "BLAZE_ROD": "minecraft:blaze_rod",
    "BONE": "minecraft:bone",
    "BROWN_MUSHROOM": "minecraft:brown_mushroom",
    "CACTUS": "minecraft:cactus",
    "CARROT_ITEM": "minecraft:carrot",
    "CHILI_PEPPER": "minecraft:skull",
    "CLAY_BALL": "minecraft:clay_ball",
    "COAL": "minecraft:coal",
    "COBBLESTONE": "minecraft:cobblestone",
    "COMPOST": "minecraft:paper",
    "CRUDE_GABAGOOL": "minecraft:skull",
    "DIAMOND": "minecraft:diamond",
    "DOUBLE_PLANT": "minecraft:double_plant",
    "EGG": "minecraft:egg",
    "EMERALD": "minecraft:emerald",
    "ENDER_PEARL": "minecraft:ender_pearl",
    "ENDER_STONE": "minecraft:end_stone",
    "FEATHER": "minecraft:feather",
    "FERMENTO": "minecraft:skull",
    "FIG_LOG": "minecraft:skull",
    "FLINT": "minecraft:flint",
    "GHAST_TEAR": "minecraft:ghast_tear",
    "GLACITE": "minecraft:packed_ice",
    "GLOWSTONE_DUST": "minecraft:glowstone_dust",
    "GOLD_INGOT": "minecraft:gold_ingot",
    "HARD_STONE": "minecraft:stone",
    "HELIANTHUS": "minecraft:skull",
    "HELIX_LOG": "minecraft:paper",
    "HEMOVIBE": "minecraft:redstone_ore",
    "HONEYCOMB": "minecraft:paper",
    "ICE": "minecraft:ice",
    "ICE_HUNK": "minecraft:paper",
    "INK_SACK": "minecraft:dye",
    "INK_SACK-3": "minecraft:dye",
    "INK_SACK-4": "minecraft:dye",
    "IRON_INGOT": "minecraft:iron_ingot",
    "LOG": "minecraft:log",
    "LOG-1": "minecraft:log",
    "LOG-2": "minecraft:log",
    "LOG-3": "minecraft:log",
    "LOG_2": "minecraft:log2",
    "LOG_2-1": "minecraft:log2",
    "LOTUS": "minecraft:paper",
    "LUSH_BERBERIS": "minecraft:double_plant",
    "MAGMA_CREAM": "minecraft:magma_cream",
    "MAGMA_FISH": "minecraft:paper",
    "MANGROVE_LOG": "minecraft:skull",
    "MELON": "minecraft:melon",
    "MITE_GEL": "minecraft:dye",
    "MITHRIL_ORE": "minecraft:prismarine_crystals",
    "MOONFLOWER": "minecraft:red_flower",
    "MUTTON": "minecraft:mutton",
    "MYCEL": "minecraft:mycelium",
    "NETHERRACK": "minecraft:netherrack",
    "NETHER_STALK": "minecraft:nether_wart",
    "OBSIDIAN": "minecraft:obsidian",
    "POISONOUS_POTATO": "minecraft:poisonous_potato",
    "PORK": "minecraft:porkchop",
    "POTATO_ITEM": "minecraft:potato",
    "PRISMARINE_CRYSTALS": "minecraft:prismarine_crystals",
    "PRISMARINE_SHARD": "minecraft:prismarine_shard",
    "PUMPKIN": "minecraft:pumpkin",
    "QUARTZ": "minecraft:quartz",
    "RABBIT": "minecraft:rabbit",
    "RABBIT_FOOT": "minecraft:rabbit_foot",
    "RABBIT_HIDE": "minecraft:rabbit_hide",
    "RAW_BEEF": "minecraft:beef",
    "RAW_CHICKEN": "minecraft:chicken",
    "RAW_FISH": "minecraft:fish",
    "RAW_FISH-1": "minecraft:fish",
    "RAW_FISH-2": "minecraft:fish",
    "RAW_FISH-3": "minecraft:fish",
    "RAW_SOULFLOW": "minecraft:paper",
    "REDSTONE": "minecraft:redstone",
    "RED_MUSHROOM": "minecraft:red_mushroom",
    "RED_ROSE": "minecraft:red_flower",
    "ROTTEN_FLESH": "minecraft:rotten_flesh",
    "ROUGH_AMBER_GEM": "minecraft:paper",
    "ROUGH_AMETHYST_GEM": "minecraft:paper",
    "ROUGH_AQUAMARINE_GEM": "minecraft:paper",
    "ROUGH_CITRINE_GEM": "minecraft:paper",
    "ROUGH_JADE_GEM": "minecraft:paper",
    "ROUGH_JASPER_GEM": "minecraft:paper",
    "ROUGH_ONYX_GEM": "minecraft:paper",
    "ROUGH_OPAL_GEM": "minecraft:paper",
    "ROUGH_PERIDOT_GEM": "minecraft:paper",
    "ROUGH_RUBY_GEM": "minecraft:paper",
    "ROUGH_SAPPHIRE_GEM": "minecraft:paper",
    "ROUGH_TOPAZ_GEM": "minecraft:paper",
    "RUBY_VEILSHROOM": "minecraft:paper",
    "SAND": "minecraft:sand",
    "SAND-1": "minecraft:sand",
    "SEA_LUMIES": "minecraft:paper",
    "SEEDS": "minecraft:wheat_seeds",
    "SHARK_FIN": "minecraft:paper",
    "SLIME_BALL": "minecraft:slime_ball",
    "SNOW_BALL": "minecraft:snowball",
    "SPECKLED_MELON": "minecraft:speckled_melon",
    "SPIDER_EYE": "minecraft:spider_eye",
    "SPONGE": "minecraft:sponge",
    "STRING": "minecraft:string",
    "SUGAR_CANE": "minecraft:reeds",
    "SULPHUR": "minecraft:gunpowder",
    "SULPHUR_ORE": "minecraft:glowstone_dust",
    "TENDER_WOOD": "minecraft:paper",
    "TITANIUM_ORE": "minecraft:paper",
    "TUNGSTEN": "minecraft:paper",
    "TURBO_GOURD": "minecraft:skull",
    "UMBER": "minecraft:paper",
    "VINESAP": "minecraft:paper",
    "WATER_LILY": "minecraft:waterlily",
    "WHEAT": "minecraft:wheat",
    "WILD_ROSE": "minecraft:double_plant",
    "WILTED_BERBERIS": "minecraft:deadbush",
    "WOOL": "minecraft:wool",
    "YELLOW_FLOWER": "minecraft:yellow_flower",
}


def _root(pid: str) -> str:
    seen = set()
    while pid in RELATIONS and pid not in seen:
        seen.add(pid)
        pid = RELATIONS[pid][0]
    return pid


def _build_trees():
    """root id -> [(item id, raw units per item)], computed from RELATIONS."""
    mult = {}

    def m(i):
        if i not in mult:
            mult[i] = 1 if i not in RELATIONS else m(RELATIONS[i][0]) * RELATIONS[i][1]
        return mult[i]

    trees = {}
    for i in set(RELATIONS) | {p for p, _ in RELATIONS.values()}:
        trees.setdefault(_root(i), []).append((i, m(i)))
    return trees


_TREES = _build_trees()
VANILLA = ROOT_VANILLA  # only chain roots may match a plain vanilla item; upgraded items always carry a custom id


def chain_of(pid: str):
    """Every item in the same upgrade tree as `pid`, with raw units per item (e.g. Seeds 1, Enchanted Seeds 160)."""
    tree = _TREES.get(_root(pid))
    return sorted(tree, key=lambda x: x[1]) if tree else [(pid, 1)]


def inv_count(product_id: str) -> int:
    """Count of a SkyBlock item in the player inventory: by custom_data id, or the vanilla item of that name."""
    return sum(it.count for it in inventory_items() if item_matches(it, product_id))


def item_matches(it, product_id: str) -> bool:
    """Exact SkyBlock item match: custom_data id equals the product, or a plain vanilla item of that name."""
    pid = product_id.upper()
    nbt = str(it.nbt) if it.nbt else ""
    if re.search(r'id:"' + re.escape(pid) + r'"', nbt):
        return True
    if pid in RELATIONS:  # upgraded items always carry a custom id, never match a plain vanilla item
        return False
    return it.item == VANILLA.get(pid, "minecraft:" + pid.lower()) and 'id:"' not in nbt


def chain_inv(pid: str) -> int:
    """Inventory total of the whole upgrade chain, in raw units."""
    return sum(inv_count(i) * mult for i, mult in chain_of(pid))


# ---------- acting on the game ----------

def menu_sig():
    """Cheap fingerprint of the open container's contents (None when no container is open)."""
    items = ms.container_get_items()
    return _note_menu(items)


def jit(x):
    """+-5% random variation on a delay."""
    return x * random.uniform(0.95, 1.05)


# ---- the in-game fast path: bb_fast.pyj runs inside the game and does clicks / closes / sign text in ~1 ms ----
FAST_PORT = _port("BB_FAST_PORT", "fast", 7779)
_fast = {"sock": None, "off_until": 0.0}
_fast_lock = threading.Lock()


def _fast_ports():
    """Where the helper may be listening. When Minescript kills the helper (it does that on a server hop), the dead helper's socket stays
    bound for as long as the game runs, so a new helper cannot take the same port: it takes the next free one of this list."""
    return [FAST_PORT] + [FAST_PORT + 2000 + 7 * k for k in range(40)]


def _fast_connect():
    s = socket.create_connection(("127.0.0.1", _fast.get("port") or FAST_PORT), 0.4)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return s


def fast_call(line: str, timeout=1.5) -> str:
    """One request to the in-game helper; raises OSError if it is not there."""
    with _fast_lock:
        for attempt in range(2):
            try:
                if _fast["sock"] is None:
                    _fast["sock"] = _fast_connect()
                s = _fast["sock"]
                s.settimeout(timeout)
                s.sendall((line + "\n").encode())
                buf = b""
                while b"\n" not in buf:
                    d = s.recv(4096)
                    if not d:
                        raise OSError("fast helper closed the connection")
                    buf += d
                return buf.split(b"\n")[0].decode()
            except OSError:
                try:
                    if _fast["sock"] is not None:
                        _fast["sock"].close()
                except Exception:
                    pass
                _fast["sock"] = None
                if attempt == 0:
                    continue
                raise


def fast_available() -> bool:
    return time.time() >= _fast["off_until"]


def fast_try(line: str):
    """The helper's reply ("ok ..."), or None when it is unavailable (then the caller uses the slow Java route)."""
    if not fast_available():
        return None
    try:
        r = fast_call(line)
        if r.startswith("ok"):
            return r
        debug_fast_error(line, r)
    except OSError:
        pass
    _fast["off_until"] = time.time() + 20   # do not retry on every click while it is down; ensure_fast() may revive it
    revive_fast_later()
    return None


def debug_fast_error(line, reply_text):
    try:
        ms.echo(f"[Blockberg] fast helper said '{reply_text}' to '{line[:40]}'")
    except Exception:
        pass


def _probe_fast() -> bool:
    """Look for a live helper on every port it may use; remember the one that answers."""
    order = [_fast.get("port")] + _fast_ports() if _fast.get("port") else _fast_ports()
    for port in dict.fromkeys(order):
        try:
            c = socket.create_connection(("127.0.0.1", port), 0.3)
            c.settimeout(0.4); c.sendall(b"ping\n")
            ok = c.recv(16).startswith(b"pong")
            c.close()
        except OSError:
            continue
        if ok:
            with _fast_lock:
                if _fast.get("port") != port:
                    try:
                        if _fast["sock"] is not None: _fast["sock"].close()
                    except Exception:
                        pass
                    _fast["sock"] = None
                    _fast["port"] = port
            _fast["off_until"] = 0.0
            return True
    return False


_fast_reviving = [0.0]


def revive_fast_later():
    """The helper stopped answering (killed on a server hop?): look for it or start a new one in the background, at most once in 40 s."""
    if time.time() - _fast_reviving[0] < 40:
        return
    _fast_reviving[0] = time.time()
    threading.Thread(target=ensure_fast, daemon=True).start()


def ensure_fast():
    """Make sure bb_fast.pyj is running in the game (start it if not). Safe to call any time."""
    if _probe_fast():
        return True
    try:
        ms.execute("\\bb_fast")
    except Exception:
        return False
    end = time.time() + 6
    while time.time() < end:
        time.sleep(0.3)
        if _probe_fast():
            return True
    return False


def menu_id():
    """Window id of the open container (2 Java reads, ~0.2 s through Minescript; ~1 ms through the fast helper)."""
    r = fast_try("cid")
    if r is not None:
        return int(r.split()[1])
    _, player, _ = _java_refs()
    return player.containerMenu.containerId


def click_slot(slot: int, button: int = 0, wait: bool = True, timeout: float = CLICK_TIMEOUT, cid=None):
    """button 0 = left click, 1 = right click. Waits until the menu actually changes (or closes) instead of
    sleeping a fixed time, so the next step never reads a stale menu."""
    before = menu_sig() if wait else None
    if wait and _msig[0] is not None:
        age = time.time() - _msig[1]
        if age < MENU_CLICK_DELAY:
            time.sleep(MENU_CLICK_DELAY - age)
    if fast_try(f"click {slot} {button}") is None:
        mc, player, gm = _java_refs()
        try:
            gm.handleContainerInput(cid if cid is not None else player.containerMenu.containerId, slot, button, _PICKUP, player)
        except Exception:
            _jv["mc"] = None   # stale handles (new world / reconnect): re-read and try once more
            mc, player, gm = _java_refs()
            gm.handleContainerInput(player.containerMenu.containerId, slot, button, _PICKUP, player)
    _last_menu_act[0] = time.time()
    tick(f"click {slot}")
    time.sleep(jit(MIN_CLICK_GAP))
    if not wait:
        return
    end = time.time() + timeout
    while time.time() < end:
        if menu_sig() != before:
            return
        time.sleep(0.03)


_NAV_BUTTONS = ("manage orders", "create buy order", "create sell offer", "custom amount", "custom price", "top order +0.1")


def press_name(text: str, exact=False, timeout=3.0, button: int = 0):
    """Click the button called `text`. Pure navigation buttons are checked: if the menu title has not changed 0.9 s after
    the click, the click was lost (Hypixel can re-open a menu under us, leaving our window id stale), so click again.
    Buttons that spend or cancel something (Buy Order, Cancel Order, ...) are never repeated."""
    title = None
    nav = text.lower() in _NAV_BUTTONS
    for attempt in range(2 if nav else 1):
        slot = wait_for(text, exact, timeout)
        if nav:
            try:
                title = ms.screen_name()
            except Exception:
                title = None
        click_slot(slot, button)
        tick(f"pressed {text}" + (" (again)" if attempt else ""))
        if not nav:
            return
        end = time.time() + 0.9
        while time.time() < end:
            try:
                if ms.screen_name() != title:
                    return
            except Exception:
                return
            time.sleep(0.03)


def close_menu():
    _last_menu_act[0] = time.time()
    if fast_try("close") is None:
        _java_refs()[1].closeContainer()
    time.sleep(0.1)


def wait_purse(before, timeout=1.5):
    """Wait for the scoreboard purse to change from `before` (it lags the trade by a moment)."""
    end = time.time() + timeout
    while time.time() < end:
        p = purse()
        if p is not None and p != before:
            return p
        time.sleep(0.08)
    return purse()


def free_slots() -> int:
    used = {it.slot for it in inventory_items() if it.slot is not None and 0 <= it.slot <= 35}
    return 36 - len(used)


def settle_inventory(max_wait=3.0, stable_for=0.8):
    """Wait until the inventory stops changing (the Personal Compactor and sacks are done absorbing items)."""
    end = time.time() + max_wait
    last, since = free_slots(), time.time()
    while time.time() < end:
        cur = free_slots()
        if cur != last:
            last, since = cur, time.time()
        elif time.time() - since >= stable_for:
            return
        time.sleep(0.1)


def ensure_clean():
    """Close any open menu/GUI so nothing is on screen while we act and while chat messages arrive."""
    for _ in range(3):
        if screen_class() == "none":
            return
        _last_menu_act[0] = time.time()
        if fast_try("close") is None:
            mc, player, _gm = _java_refs()
            try:
                player.closeContainer()
            except Exception:
                _jv["mc"] = None
                try:
                    _java_refs()[1].closeContainer()
                except Exception:
                    pass
        time.sleep(0.08)
        if screen_class() != "none":
            _java_refs()[0].setScreen(null)   # (mc is only defined on the slow route above: use the cached handle)
            time.sleep(0.08)


@contextlib.contextmanager
def action():
    """Exclusive in-game action: everything closed before and after."""
    with _lock:
        ensure_clean()
        try:
            yield
        finally:
            try:
                ensure_clean()
            except Exception:
                pass


def type_in_sign(text: str):
    """Type `text` into the open sign editor (Hypixel's amount/price prompt) and confirm it."""
    deadline = time.time() + 3
    while "Sign" not in screen_class():
        if time.time() > deadline:
            raise RuntimeError(f"no sign editor open (screen: {screen_class()})")
        time.sleep(0.05)
    tick("sign open")
    time.sleep(SIGN_DELAY)   # a sign confirmed the instant it opens is ignored, like a menu click
    _fr = fast_try(f"settext {text}")
    if _fr is not None:
        time.sleep(0.06)   # the game needs a tick to take the text before the sign is closed
        _fr = fast_try("done")
    tick("sign text sent" + ("" if _fr is not None else " (slow route)"))
    if _fr is None:
        scr = _java_refs()[0].screen
        try:
            scr.setMessage(text)   # sets the whole line in one call (typing char by char costs two Java calls per character)
        except Exception:
            for ch in text:
                scr.charTyped(CharacterEvent(ord(ch)))
                time.sleep(0.004)
        time.sleep(0.05)
        scr.onClose()  # closing the sign editor sends the text to the server
    end = time.time() + 2.0
    while time.time() < end and "Sign" in screen_class():
        time.sleep(0.03)
    tick("sign closed")
    end = time.time() + 1.2  # the next menu arrives from the server a moment later
    while time.time() < end and menu_sig() is None:
        time.sleep(0.03)
    tick("sign done")


def display(product_id: str) -> str:
    """Bazaar display name for a product id: SEEDS -> Seeds, ENCHANTED_COAL -> Enchanted Coal."""
    return NAMES.get(product_id.upper()) or product_id.replace("_", " ").title()


def page_product_name() -> str:
    """Name of the product whose bazaar page is open (the centre item)."""
    for it in ms.container_get_items() or []:
        if it.slot == 13:
            return item_name(it)
    return ""


def open_product(product: str):
    """Open a product's bazaar page. Strict: only an exact name match is accepted, and the opened page
    is verified, so a search like 'seeds' can never land on 'Box of Seeds'."""
    want = display(product).lower()
    menu_gap()
    ms.execute(f"/bz {want}")
    resent, t_cmd = False, time.time()
    deadline, last, stable = time.time() + 5, None, 0
    while time.time() < deadline:  # wait for the product page, or a search page that has stopped changing
        sig = menu_sig()
        if sig is None and not resent and time.time() - t_cmd > 1.8:
            resent = True   # Hypixel drops a command sent too soon after the last one: send it again once
            ms.execute(f"/bz {want}")
        if sig is not None:
            if slot_with("Create Buy Order") is not None:
                break
            stable = stable + 1 if sig == last else 0
            if stable >= 2:
                break
            last = sig
        time.sleep(0.06)
    tick("opened /bz")
    if slot_with("Create Buy Order") is None:  # a search results page: click the exact match
        items = [it for it in ms.container_get_items() or [] if it.slot < 54]
        hits = [it for it in items if item_name(it).lower() == want]
        if not hits:
            hits = [it for it in items if want in item_name(it).lower()]
            if len(hits) != 1:
                names = [item_name(it) for it in hits][:5]
                ensure_clean()
                if not items and where() != "skyblock":   # nothing opened at all: limbo or a lobby, not a wrong name
                    _rejoin_hint[0] = time.time()
                    raise RuntimeError("the bazaar is not available: the character is not in SkyBlock (limbo or a lobby); it is being sent back")
                raise RuntimeError(f"/bz search '{want}' has no exact match (candidates: {names})")
        click_slot(hits[0].slot)
        wait_for("Create Buy Order", timeout=4)
    title = page_product_name()
    if title.lower() != want:
        ensure_clean()
        raise RuntimeError(f"opened bazaar page '{title}' but expected '{display(product)}'")


# ---------- chat events (read from the game log) ----------

def chat_loop():
    """Watch the game log for "[Bazaar] ..." lines.

    Minescript's chat listener does not see Hypixel's system messages, but they do appear in
    logs/latest.log as "[System] [CHAT] [Bazaar] ...", so tail that file instead."""
    from pathlib import Path
    path = Path(__file__).resolve().parent.parent / "logs" / "latest.log"
    with open(path, "r", errors="ignore") as f:
        f.seek(0, 2)
        while True:
            line = f.readline()
            if not line:
                time.sleep(0.1)
                continue
            if "[CHAT]" in line:
                raw = line.split("[CHAT]", 1)[1].strip()
                if raw and "[Blockberg]" not in raw:
                    for c in list(_chat_subs):
                        try:
                            send(c, {"type": "chat", "text": raw, "ts": time.time()})
                        except OSError:
                            _chat_subs.discard(c)
            if "[CHAT]" in line and "[Blockberg]" not in line:
                _syslog.append((time.time(), strip_fmt(line.split("[CHAT]", 1)[1]).strip()))
                del _syslog[:-200]
            if "[CHAT]" in line and "[Sacks]" in line:
                m = re.search(r"\[Sacks\]\s*([+-])\s*([\d,]+)\s*items?", line)
                if m:
                    _sacklog.append((time.time(), (1 if m.group(1) == "+" else -1) * int(m.group(2).replace(",", ""))))
                    del _sacklog[:-100]
            if "[CHAT]" in line and "[Bazaar]" in line:
                text = strip_fmt(line.split("[CHAT]", 1)[1]).strip()
                _chat.append((time.time(), text))
                del _chat[:-100]
                push({"type": "bazaar", "msg": text})


def send(conn, obj):
    """Write one JSON line to a client; safe to call from several threads."""
    data = (json.dumps(obj) + "\n").encode()
    with _wlock.setdefault(conn, threading.Lock()):
        conn.sendall(data)


def push(obj):
    for c in list(_clients):
        try:
            send(c, obj)
        except OSError:
            _clients.remove(c)


def sack_chat_delta(since: float, timeout=8.0, quiet=2.5) -> int:
    """Net sack item change reported by Hypixel's "[Sacks] +N items" lines after `since`.
    The lines are batched over 5 s, so wait for the first one, then until none has arrived for `quiet` seconds.
    The count is in sack units (e.g. 2,240 Seeds compact to '+14 items' of Enchanted Seeds); there is no item name,
    so this proves 'something moved in this direction', not which item."""
    end = time.time() + timeout
    last_seen = None
    while time.time() < end:
        hits = [(ts, d) for ts, d in list(_sacklog) if ts >= since]
        if hits:
            last_seen = max(ts for ts, _ in hits)
            if time.time() - last_seen >= quiet:
                break
        time.sleep(0.2)
    return sum(d for ts, d in list(_sacklog) if ts >= since)


def expect_syslog(since: float, needles, timeout=4.0) -> str:
    """Like expect_chat, but over every chat line (bank, cookie and other non-[Bazaar] messages)."""
    end = time.time() + timeout
    while time.time() < end:
        for ts, text in list(_syslog):
            if ts >= since and any(n in text for n in needles):
                return text
        time.sleep(0.05)
    return ""


def expect_chat(since: float, needles, timeout=4.0) -> str:
    """Wait for a [Bazaar] chat line after `since` containing any of `needles`."""
    end = time.time() + timeout
    while time.time() < end:
        for ts, text in list(_chat):
            if ts >= since and any(n in text for n in needles):
                tick("chat: " + text[:40])
                return text
        time.sleep(0.05)
    return ""


# ---------- inventory: into the sacks / onto the bazaar ----------

def insert_into_sacks(repeat=False):
    """/sacks -> the "Insert inventory" button: everything a sack can take moves out of the inventory. Returns how many items moved.
    One click can leave items behind (a click that arrives while the menu is still loading, or items that appeared while it ran), so after a click
    that moved something and left items in the inventory the button is pressed again, up to three times; a click that moves nothing ends it.
    Only the Insert-inventory button of the app asks for that (`repeat`): the claim loops need one quick press per round."""
    count = lambda: sum(it.count for it in inventory_items() if it.slot is not None and 0 <= it.slot <= 35)
    before = prev = count()
    for rnd in range(3 if repeat else 1):
        ensure_clean()
        menu_gap()
        ms.execute("/sacks")
        slot = wait_for("Insert inventory", timeout=4)
        click_slot(slot)
        time.sleep(0.5)
        settle_inventory(2.5, 0.5)
        ensure_clean()
        now = count()
        if now >= prev or now == 0:
            prev = now
            break
        prev = now
    return max(0, before - prev)


def sell_inventory(prices: dict):
    """Put every bazaar item in the inventory on the bazaar as a sell offer at the price given for it (the app sends best offer - 0.1).
    One offer per item type; a sell offer always lists the whole inventory stack. Returns (placed, failed, skipped)."""
    placed, failed, skipped = [], [], []
    t_all = time.time()
    for it in inv_all()["items"]:
        pid, n = it["id"], it["count"]
        price = prices.get(pid)
        if not price or float(price) < 0.1 or n <= 0:
            skipped.append(pid)
            continue
        try:
            selloffer(pid, n, float(price))
            placed.append({"id": pid, "qty": n, "price": float(price)})
        except Exception as e:
            try:
                ensure_clean()
            except Exception:
                pass
            failed.append({"id": pid, "err": str(e)[:110]})
            if any("cooldown" in t.lower() for ts, t in list(_syslog) if ts >= t_all):
                failed.append({"id": "(rest)", "err": "Hypixel put order placing on cooldown: the remaining items were not tried"})
                break
    return placed, failed, skipped


# ---------- stash and Lost & Found ----------

def pickup_stash(max_secs=180):
    """Take everything out of the stash. One /pickupstash only moves what fits in the inventory, so this repeats: after each pickup it
    gives the compactor and the sacks a moment to clear the inventory, makes room with /sacks "Insert inventory" if the game says the
    inventory is full, and goes on until the stash is empty, nothing more can be taken, or `max_secs` have passed."""
    t_start = time.time()
    total, rounds, stalls, left, said_all, cooldowns = 0, 0, 0, None, False, 0
    while time.time() - t_start < max_secs:
        r0 = time.time()
        for cmd in (("/pickupstash", "/pickupstash material", "/pickupstash item") if rounds == 0 else ("/pickupstash material",)):
            menu_gap()
            ms.execute(cmd)
            time.sleep(1.6)   # /pickupstash has a cooldown of about a second: a command sent sooner is refused
        rounds += 1
        new = [t for ts, t in list(_syslog) if ts >= r0]
        low = " | ".join(new).lower()
        got = sum(int(m.group(1).replace(",", "")) for t in new for m in [re.search(r"picked up ([\d,]+)", t)] if m)
        total += got
        l = [t for t in new if "still have" in t.lower()]
        if l:
            left = l[-1].strip()
        all_mat = any("picked up all" in t.lower() and "material" in t.lower() for t in new)
        if got == 0 and ("isn't holding any" in low or "not holding any" in low):   # the game says the stash is empty: nothing to wait for
            said_all = True
            break
        if all_mat or ("already empty" in low and got == 0 and rounds > 1):
            said_all = True
            break
        if got == 0 and "cooldown" in low and cooldowns < 12:
            cooldowns += 1   # refused for being too soon: wait and ask again (not a reason to stop)
            time.sleep(1.5)
            rounds -= 1
            continue
        if got == 0:
            if "inventory is full" in low:
                stalls += 1
                if stalls >= 4:
                    break
                try:
                    insert_into_sacks()          # whatever the sacks take leaves the inventory
                except Exception:
                    ensure_clean()
                settle_inventory(6.0, 1.0)       # the compactor + sacks get time to work
                continue
            break
        stalls = 0
        if not l and rounds > 1:   # a material pickup that leaves nothing behind: done (round one also ran the item-stash command)
            said_all = True
            break
        settle_inventory(6.0, 1.0)
    secs = round(time.time() - t_start)
    if total == 0:
        return "inventory full: nothing could be taken (make room, or turn on a compactor / sacks for these items)" if stalls else "nothing in the stash"
    tail = " - the stash is now empty" if said_all else (f" - still left: {left[:70]}" if left else "")
    if not said_all:
        tail += f" [stopped: {' | '.join(t for t in new)[:200]}]"
    return f"took {total:,} item(s) in {rounds} round(s), {secs}s{tail}"


_LF_SKIP = ("close", "go back", "next page", "previous page", "back", "")


def pickup_lost_found():
    """Open the SkyBlock menu; if its Lost & Found slot is there (it only appears when something is waiting), click through every
    pickup in it. The result says what it saw, so a run that finds nothing still tells you where it looked."""
    ensure_clean()
    menu_gap()
    ms.execute("/sbmenu")
    end = time.time() + 3.5
    slot, seen = None, []
    while time.time() < end:
        items = [it for it in ms.container_get_items() or [] if it.slot < 54]
        if items:
            seen = sorted((it.slot, item_name(it)) for it in items if item_name(it).strip())
            for it in items:
                text = (item_name(it) + " " + item_lore(it)).lower()
                if "lost" in text and ("found" in text or "pickup" in text) or "available pickups" in text:
                    slot = it.slot
                    break
            if slot is not None or time.time() > end - 2.0:
                break
        time.sleep(0.1)
    if slot is None:
        ensure_clean()
        return "nothing in Lost & Found (the SkyBlock menu showed no Lost & Found slot; it only shows when something is waiting)"
    click_slot(slot)
    time.sleep(0.5)
    title = ms.screen_name() or ""
    if "lost" not in title.lower() and "found" not in title.lower():
        shown = ", ".join(n for _, n in sorted((it.slot, item_name(it)) for it in ms.container_get_items() or [] if it.slot < 54 and item_name(it).strip()))[:160]
        ensure_clean()
        return f"could not open Lost & Found (it opened '{title}' with: {shown})"
    taken, same, last, names = 0, 0, None, []
    for _ in range(80):
        items = [it for it in ms.container_get_items() or [] if it.slot < 54 and item_name(it).strip().lower() not in _LF_SKIP and it.item != "minecraft:air"
                 and "glass_pane" not in it.item and "barrier" not in it.item]
        if not items:
            break
        sig = tuple(sorted((it.slot, item_name(it)) for it in items))
        same = same + 1 if sig == last else 0
        if same >= 3:
            ensure_clean()
            return f"took {taken} pickup(s) ({', '.join(names[:4])}); the rest did not come out (inventory full?)"
        last = sig
        names.append(item_name(items[0])[:30])
        click_slot(items[0].slot, wait=False)
        taken += 1
        time.sleep(0.5)
        t2 = (ms.screen_name() or "").lower()
        if "lost" not in t2 and "found" not in t2:
            break
    ensure_clean()
    return f"took {taken} pickup(s) from Lost & Found ({', '.join(names[:4])})" if taken else "Lost & Found was open but had nothing to take"


# ---------- trades ----------

def guard_warning(what: str, since: float):
    """Hypixel shows a "WARNING"/"Confirm" screen when a trade is far from the average price.
    Never auto-confirm it. (A successful trade leaves the menu open too, so look for the warning itself,
    and trust the "[Bazaar] Bought/Sold" chat line when it arrives.)"""
    end = time.time() + 2.0
    while time.time() < end:
        if expect_chat(since, ["Bought", "Sold"], timeout=0.0):
            return
        if menu_open() and (slot_with("WARNING") is not None or slot_with("Confirm", exact=True) is not None):
            ensure_clean()
            raise RuntimeError(f"{what} blocked: Hypixel price warning (far from average price). Not confirmed.")
        time.sleep(0.05)


def instabuy(product, qty):
    """Instant-buy `qty`. 1 and 64 use the preset buttons; any other amount uses Custom Amount
    (type it into the sign, then click the 'Confirm Instant Buy' button)."""
    qty = int(qty)
    if not 1 <= qty <= MAX_UNITS:
        raise RuntimeError(f"qty must be 1..{MAX_UNITS}")
    t0 = time.time()
    open_product(product)
    press_name("Buy Instantly")
    if qty in (1, STACK):
        press_name("Buy only" if qty == 1 else "Buy a stack")
    else:
        press_name("Custom Amount")
        type_in_sign(str(qty))
        end = time.time() + 4  # the confirm screen has one 'Custom Amount' button, in slot 13
        while time.time() < end and not any(it.slot == 13 and item_name(it) == "Custom Amount"
                                            for it in ms.container_get_items() or []):
            time.sleep(0.1)
        if not any(it.slot == 13 and item_name(it) == "Custom Amount" for it in ms.container_get_items() or []):
            ensure_clean()
            raise RuntimeError("Confirm Instant Buy screen never appeared")
        click_slot(13)
    guard_warning("insta-buy", t0)
    ensure_clean()
    msg = expect_chat(t0, ["Bought", "enough inventory space"])
    if msg and "enough inventory space" in msg:
        raise RuntimeError(f"NO_SPACE: Hypixel refused: not enough inventory space for {qty:,} (an instant buy goes to the inventory)")
    if not msg:
        raise RuntimeError("no '[Bazaar] Bought' confirmation from Hypixel")
    return msg


def instabuy_rounds(product, qty):
    """Instant-buy more than the inventory holds: buy what fits, put it in the sacks, repeat."""
    qty = int(qty)
    if qty <= free_slots() * STACK:
        return instabuy(product, qty)
    left, got, coins, rounds, name = qty, 0, 0.0, 0, display(product)
    while left > 0:
        fit = free_slots() * STACK
        if fit < STACK:
            insert_into_sacks(repeat=True)
            ensure_clean()
            fit = free_slots() * STACK
            if fit < STACK:
                break
        n = min(left, fit, MAX_UNITS)
        m = instabuy(product, n)
        rounds += 1
        mm = re.search(r"Bought ([\d,]+)x .*? for ([\d,.]+) coins", m)
        got += int(mm.group(1).replace(",", "")) if mm else n
        coins += float(mm.group(2).replace(",", "")) if mm else 0.0
        left -= n
        insert_into_sacks(repeat=True)   # empty the inventory into the sacks for the next round
        ensure_clean()
    if got == 0:
        raise RuntimeError("NO_SPACE: the inventory is full and the sacks took nothing, so nothing could be bought")
    tail = "" if got >= qty else f" (only {got:,} of {qty:,}: inventory and sacks are full)"
    return f"[Bazaar] Bought {got:,}x {name} for {coins:,.1f} coins! ({rounds} rounds, put in sacks){tail}"


def instasell(product, qty):
    open_product(product)
    t0 = time.time()
    press_name("Sell Instantly")
    guard_warning("insta-sell", t0)
    msg = expect_chat(t0, ["Sold"])
    if not msg:
        raise RuntimeError("no '[Bazaar] Sold' confirmation from Hypixel (nothing to sell?)")
    return msg


def check_confirm_screen(button: str):
    """Hypixel flags bad prices on the confirm screen ('Underbidding too much!', 'Overbidding too much!') and then
    ignores the click. Read that warning first so a rejected price fails at once, with the real reason."""
    slot = wait_for(button, exact=True)
    lore = next((item_lore(it) for it in ms.container_get_items() or [] if it.slot == slot), "")
    m = re.search(r"(?:Underbidding|Overbidding|Undercutting|Overpricing)[^!]*!|(?:Cannot|Can't|Unable)[^!]*!", lore)
    if m:
        ensure_clean()
        raise RuntimeError(f"Hypixel rejects this price: {m.group(0).strip()}")
    for it in ms.container_get_items() or []:   # the confirm screen shows the price the order will really get
        pm = re.search(r"Price per unit:\s*([\d,\.]+)", item_lore(it))
        if pm:
            return num(pm.group(1))
    return None


def fmt_price(p) -> str:
    return f"{round(float(p), 1):.1f}"


_last_buy_unit = [None]   # price per unit of the buy order that was just placed


def buyorder(product, qty, price, top_plus=False, max_price=None):
    """top_plus: use Hypixel's own "Top Order +0.1" button instead of typing a price (one sign fewer, and the price is
    the real top bid at this very moment). max_price then guards it: the order is not placed above that."""
    qty = int(qty)
    if not 1 <= qty <= 71680:
        raise RuntimeError("buy order qty must be 1..71,680 (the bazaar's limit)")
    open_product(product)
    t0 = time.time()
    press_name("Create Buy Order")
    press_name("Custom Amount")
    type_in_sign(str(qty))
    use_top = top_plus
    if top_plus:
        # With no buy orders on the item yet (a bid of 0), Hypixel has no "Top Order +0.1" button. Then type the price instead.
        try:
            wait_for("Top Order +0.1", exact=True, timeout=1.5)
        except RuntimeError:
            if price is None:
                ensure_clean()
                raise
            use_top = False
            if max_price is not None and float(price) > float(max_price) + 1e-9:
                ensure_clean()
                raise RuntimeError(f"no top order to follow and {float(price):g} is above your max {float(max_price):g}")
    if use_top:
        press_name("Top Order +0.1", exact=True)
    else:
        press_name("Custom Price")
        type_in_sign(fmt_price(price))
    unit = check_confirm_screen("Buy Order")
    _last_buy_unit[0] = unit
    if top_plus and max_price is not None and unit is not None and unit > float(max_price) + 1e-9:
        ensure_clean()
        raise RuntimeError(f"top order +0.1 is {unit:g}, above your max {float(max_price):g}")
    press_name("Buy Order", exact=True)  # exact: "Cancel Buy Order" also contains the words
    msg = expect_chat(t0, ["Buy Order Setup", "maximum of"], timeout=5)
    if msg and "maximum of" in msg:
        close_menu()
        raise RuntimeError("the bazaar's order limit is reached (" + msg.strip() + "): claim or cancel orders first. In a co-op the limit counts every member's orders")
    if not msg:
        close_menu()
        raise RuntimeError("no '[Bazaar] Buy Order Setup' confirmation (price rejected?)")
    return msg


def selloffer(product, qty, price):
    """Hypixel sell offers always list the *entire* inventory stack of the item; qty is not used."""
    open_product(product)
    t0 = time.time()
    press_name("Create Sell Offer")
    press_name("Custom Price")
    type_in_sign(fmt_price(price))
    check_confirm_screen("Sell Offer")
    press_name("Sell Offer", exact=True)
    msg = expect_chat(t0, ["Sell Offer Setup"], timeout=5)
    if not msg:
        close_menu()
        raise RuntimeError("no '[Bazaar] Sell Offer Setup' confirmation (no items in inventory?)")
    return msg


# ---------- sacks ----------

_sack_map = {}  # item display name (lowercase) -> sack name that holds it


def parse_amount(text: str) -> float:
    m = re.match(r"([\d,\.]+)\s*([kKmMbB]?)", text.strip())
    if not m:
        return 0.0
    mult = {"": 1, "k": 1e3, "m": 1e6, "b": 1e9}[m.group(2).lower()]
    return num(m.group(1)) * mult


def sacks_menu():
    """Open the 'Sack of Sacks' menu and return {unique sack name: first slot}."""
    ensure_clean()
    menu_gap()
    ms.execute("/sacks")
    deadline = time.time() + 4
    sacks = {}
    while time.time() < deadline and not sacks:
        for it in ms.container_get_items() or []:
            n = item_name(it)
            if n.endswith(" Sack") and it.item == "minecraft:player_head":
                sacks.setdefault(n, it.slot)
        time.sleep(0.15)
    return sacks


def read_sack_item(display_name: str):
    """In an open sack: the stored count of `display_name`, or None if this sack doesn't hold it."""
    for it in ms.container_get_items() or []:
        if item_name(it).lower() == display_name.lower():
            m = re.search(r"Stored:\s*([\d,\.]+\s*[kKmMbB]?)", item_lore(it))
            return int(parse_amount(m.group(1))) if m else 0
    return None


def in_sack() -> bool:
    """True when a sack's contents are showing (its Go Back button leads to the Sack of Sacks)."""
    return any(item_name(it) == "Go Back" and "Sack of Sacks" in item_lore(it)
               for it in ms.container_get_items() or [])


def sack_count(display_name: str):
    """Total stored in the sacks for an item (e.g. 'Coal'); None if no sack holds it.
    The first lookup scans the sacks and remembers which sack holds the item; later ones go straight there.
    Each sack is opened from a freshly opened /sacks menu, so one failure can't derail the rest."""
    key = display_name.lower()
    names = list(sacks_menu())
    if not names:
        ensure_clean()
        return None
    if key in _sack_map and _sack_map[key] in names:
        names = [_sack_map[key]]
    elif key.startswith("enchanted"):
        names.sort(key=lambda n: "Enchanted" not in n)
    for i, name in enumerate(names):
        sacks = sacks_menu() if i else sacks_menu_current()
        if name not in sacks:
            continue
        click_slot(sacks[name], 1)  # right-click opens a sack
        end = time.time() + 3
        while time.time() < end and not in_sack():
            time.sleep(0.1)
        if not in_sack():
            continue
        val = read_sack_item(display_name)
        if val is not None:
            _sack_map[key] = name
            ensure_clean()
            return val
    ensure_clean()
    return None


def sacks_menu_current():
    """Sacks of the already-open Sack of Sacks menu (no /sacks command)."""
    sacks = {}
    for it in ms.container_get_items() or []:
        n = item_name(it)
        if n.endswith(" Sack") and it.item == "minecraft:player_head":
            sacks.setdefault(n, it.slot)
    return sacks


def sack_scan(max_pages=8):
    """Open each distinct sack once and list what it can hold. Sacks with the same name add their capacity
    ("Multiple sacks sum their capacity"), so we report how many copies there are and the capacity of each."""
    names = {}   # sack name -> {"copies": n, "cap_each": per-item capacity of one copy}
    for it in (sacks_menu() and ms.container_get_items()) or []:
        n = item_name(it)
        if n.endswith(" Sack") and it.item == "minecraft:player_head":
            e = names.setdefault(n, {"copies": 0, "cap_each": 0})
            e["copies"] += 1
            m = re.search(r"Capacity:\s*([\d,\.]+\s*[kKmMbB]?)\s*of each", item_lore(it))
            if m:
                e["cap_each"] = int(parse_amount(m.group(1)))
    out = []
    cur = sacks_menu_current()   # the Sack of Sacks menu is already open from the read above: no close and re-open
    for name, e in names.items():
        slot = cur.get(name)
        if slot is None:
            cur = sacks_menu()   # not on screen (menu closed under us): open it again
            slot = cur.get(name)
        if slot is None:
            continue
        click_slot(slot, 1)
        end = time.time() + 3
        while time.time() < end and not in_sack():
            time.sleep(0.1)
        if not in_sack():
            continue
        items, seen = [], set()
        for _ in range(max_pages):
            for it in ms.container_get_items() or []:
                m = re.search(r"Stored:\s*([\d,\.]+\s*[kKmMbB]?)", item_lore(it))
                n = item_name(it)
                if m and it.slot < 54 and n not in seen:
                    seen.add(n)
                    items.append({"name": n, "stored": int(parse_amount(m.group(1)))})
            nxt = slot_with("Next Page")
            if nxt is None:
                break
            click_slot(nxt)
        out.append({"name": name, "copies": e["copies"], "cap_each": e["cap_each"], "items": items})
        # back to the Sack of Sacks with the sack's own "Go Back" button instead of closing the menu and typing /sacks again
        cur = {}
        back = slot_with("Go Back")
        if back is not None:
            click_slot(back)
            end = time.time() + 2
            while time.time() < end and not cur:
                cur = sacks_menu_current()
                time.sleep(0.08)
    ensure_clean()
    return {"type": "sacks", "sacks": out}


def chain_sacks(pid: str):
    """Sack total of the whole upgrade chain, in raw units (None if no sack holds any of it)."""
    vals = [(sack_count(display(i)), mult) for i, mult in chain_of(pid)]
    known = [(v, m) for v, m in vals if v is not None]
    return sum(v * m for v, m in known) if known else None


def holdings(product_id: str):
    chain = chain_of(product_id)
    detail = {i: {"inventory": inv_count(i), "sacks": sack_count(display(i))} for i, _ in chain}
    return {"inventory": chain_inv(product_id), "sacks": chain_sacks(product_id), "chain": detail}


# ---------- moving goods: sacks -> inventory -> NPC shop ----------

def sack_get(pid: str, qty: int):
    """Pull items out of the sacks into the inventory with /gfs; returns how many arrived."""
    before = inv_count(pid)
    for arg in (pid, display(pid).lower()):
        ms.execute(f"/gfs {arg} {int(qty)}")
        end = time.time() + 2.5
        while time.time() < end:
            if inv_count(pid) > before:
                settle_inventory(1.5, 0.3)
                return inv_count(pid) - before
            time.sleep(0.08)
    raise RuntimeError(f"/gfs {pid} {qty} moved nothing (none in sacks, or inventory full?)")


def pull_from_sacks(pid: str, qty):
    """Selling "from sacks": top the inventory up to `qty` from the sacks first (a no-op if it already holds enough)."""
    have = inv_count(pid)
    need = int(qty or 0) - have
    if need > 0:
        try:
            sack_get(pid, need)
        except RuntimeError:
            if have == 0:
                raise


TRADES_CLICK_INTERVAL = 0.1    # seconds between sale clicks, without waiting for the menu after each one (benchmarked: 0.1 s sells 2,000 carrots in 13 s instead of 20 s; going lower gains nothing)


def trades_sell(pid: str, max_stacks=None, interval=None):
    """Open /trades and sell every inventory stack of exactly `pid` by clicking it (the shop buys what you click).
    interval: click every `interval` seconds without waiting for the menu to answer; stacks that did not sell (a click can be dropped) are clicked again."""
    interval = interval if interval is not None else TRADES_CLICK_INTERVAL
    pu0, inv0 = purse(), inv_count(pid)
    if inv0 == 0:
        raise RuntimeError(f"no {display(pid)} in the inventory to sell")
    menu_gap()
    ms.execute("/trades")
    # Recognise the menu by its 'Trades' header item. The 'Sell Item' button is NOT reliable: after any sale,
    # slot 49 becomes a BUYBACK button for the last item sold, which would cost coins if clicked.
    end = time.time() + 4
    while time.time() < end and not any(it.slot == 4 and item_name(it) == "Trades" for it in ms.container_get_items() or []):
        time.sleep(0.04)
    if not any(it.slot == 4 and item_name(it) == "Trades" for it in ms.container_get_items() or []):
        ensure_clean()
        raise RuntimeError("/trades menu did not open")
    # Only ever click the player's own inventory slots (>= 54), never the shop's slots (buyback lives there).
    slots = [it.slot for it in ms.container_get_items() or [] if it.slot >= 54 and item_matches(it, pid)]
    if max_stacks:
        slots = slots[: int(max_stacks)]
    if interval:
        for _pass in range(4):
            for sl in slots:
                assert sl >= 54, "refusing to click a shop slot"
                click_slot(sl, wait=False)
                time.sleep(interval)
            time.sleep(0.5)   # let the last sales register, then look for stacks that are still there
            slots = [it.slot for it in ms.container_get_items() or [] if it.slot >= 54 and item_matches(it, pid)]
            if max_stacks:
                break
            if not slots:
                break
    else:
        for sl in slots:
            assert sl >= 54, "refusing to click a shop slot"
            click_slot(sl)  # waits for the menu to change, i.e. for the sale to register
    ensure_clean()
    pu1 = wait_purse(pu0)
    inv1 = inv_count(pid)
    sold = inv0 - inv1
    if sold <= 0:
        raise RuntimeError(f"/trades did not take any {display(pid)} (the shop may not buy it)")
    gained = (pu1 - pu0) if (pu0 is not None and pu1 is not None) else None
    return sold, gained, {"inventory": True, "purse": None if gained is None else gained > 0}


def inv_room(pid: str) -> int:
    """How many more of `pid` fit in the inventory: free slots plus room in its part-full stacks."""
    room = 0
    for it in inventory_items():
        if it.slot is not None and 0 <= it.slot <= 35 and item_matches(it, pid):
            room += max(0, STACK - it.count)
    return free_slots() * STACK + room


def trades_sell_amount(pid: str, qty=None, mode="qty", max_rounds=30, use_sacks=True, interval=None):
    """Sell `pid` to the NPC shop at /trades, pulling from the sacks (/gfs) whenever the inventory is short.
      mode "qty":  sell about `qty` items (the shop takes whole stacks, so it rounds up to stacks)
      mode "fill": fill the inventory from the sacks once, then sell everything of the item in it
      mode "all":  repeat fill-and-sell until the sacks have none left
    Returns (sold, coins gained or None, rounds)."""
    sold_total, gained_total, rounds = 0, 0.0, 0
    gained_known = True
    while rounds < max_rounds:
        have = inv_count(pid)
        if mode == "qty":
            want = int(qty) - sold_total
            if want <= 0:
                break
            pull = min(max(0, want - have), inv_room(pid))
        else:
            pull = inv_room(pid)
        if pull > 0 and use_sacks:
            try:
                sack_get(pid, pull)
            except RuntimeError:
                pass  # nothing (more) in the sacks: sell what the inventory has
            ensure_clean()
        have = inv_count(pid)
        if have == 0:
            break
        stacks = None
        if mode == "qty":
            stacks = max(1, -(-min(have, int(qty) - sold_total) // STACK))
        sold, gained, _ = trades_sell(pid, stacks, interval)
        sold_total += sold
        if gained is None:
            gained_known = False
        else:
            gained_total += gained
        rounds += 1
        if mode == "fill":
            break
        if mode == "all" and pull == 0:
            break  # sacks are empty and the inventory is sold
    if sold_total == 0:
        raise RuntimeError(f"no {display(pid)} in the inventory or sacks to sell")
    return sold_total, (gained_total if gained_known else None), rounds


# ---------- bulk selling ----------

def parse_sell_preview(lore: str):
    """'100 x Seeds for 10.0 coins  2 x Coal for 6.2 coins  You earn: 16.0 coins' -> items, total."""
    items = [{"qty": int(parse_amount(q)), "name": n.strip(), "coins": num(c)}
             for q, n, c in re.findall(r"([\d,\.]+\s*[kKmM]?)\s*x\s+(.+?)\s+for\s+([\d,\.]+)\s*coins", lore)]
    m = re.search(r"You earn:\s*([\d,\.]+)", lore)
    return items, (num(m.group(1)) if m else None)


def open_bazaar_main():
    ms.execute("/bz")
    wait_for("Sell Inventory Now", timeout=4)


def bulk_preview(what="inventory"):
    """What 'Sell Inventory Now' / 'Sell Sacks Now' would sell and earn. Opens the bazaar main menu; read-only."""
    btn = "Sell Inventory Now" if what == "inventory" else "Sell Sacks Now"
    open_bazaar_main()
    lore = next((item_lore(it) for it in ms.container_get_items() or [] if item_name(it) == btn), "")
    return parse_sell_preview(lore)


def bulk_sell(what, allow):
    """Sell everything the bazaar's bulk button lists, but ONLY if every listed item is on the allow-list.
    The button sells all sellable items it shows, so refusing unknown ones is what keeps valuables safe."""
    btn = "Sell Inventory Now" if what == "inventory" else "Sell Sacks Now"
    open_bazaar_main()
    slot = wait_for(btn, exact=True)
    lore = next((item_lore(it) for it in ms.container_get_items() or [] if it.slot == slot), "")
    items, earn = parse_sell_preview(lore)
    if not items:
        ensure_clean()
        raise RuntimeError(f"{btn}: nothing to sell")
    allowed = {a.lower() for a in allow}
    extra = [i["name"] for i in items if i["name"].lower() not in allowed]
    if not allowed or extra:
        ensure_clean()
        raise RuntimeError(f"bulk sell refused, not on the allow-list: {extra or 'no allow-list given'}")
    t0, pu0 = time.time(), purse()
    click_slot(slot)
    # Hypixel asks to confirm: "Selling whole inventory / You sell N x products / You earn X coins / Click to confirm!"
    cslot, clore, end = None, "", time.time() + 3
    while time.time() < end and cslot is None:
        for it in ms.container_get_items() or []:
            if item_name(it).startswith("Selling whole"):
                cslot, clore = it.slot, item_lore(it)
        time.sleep(0.05)
    if cslot is None:
        ensure_clean()
        raise RuntimeError("bulk sell: the confirm screen never appeared")
    m = re.search(r"You earn:\s*([\d,\.]+)", clore)
    c_earn = num(m.group(1)) if m else None
    if earn and c_earn is not None and abs(c_earn - earn) > max(0.15 * earn, 2):
        ensure_clean()  # prices moved between preview and confirm: do not sell at a different value than we checked
        raise RuntimeError(f"confirm screen earns {c_earn:,.1f} but the preview said {earn:,.1f}; not confirming")
    t1 = time.time()
    click_slot(cslot)
    guard_warning("bulk sell", t1)
    msg = expect_chat(t0, ["Sold"], timeout=3)
    gained = None
    pu1 = wait_purse(pu0, 2.0)
    if pu0 is not None and pu1 is not None:
        gained = pu1 - pu0
    ensure_clean()
    if gained is not None and earn and gained < earn * 0.5:
        raise RuntimeError(f"bulk sell paid {gained:,.1f} but the preview said {earn:,.1f}")
    return msg or "bulk sell sent", items, earn, gained


# ---------- order management ----------

def open_orders(product=None):
    """Open the 'Your Bazaar Orders' menu. The bazaar main menu (plain /bz) has the Manage Orders book itself,
    so no product page is needed; a product is only opened as a fallback if the book is not there.
    Fast actions can outrun Hypixel (a command or click that arrives too soon is dropped), so each step is retried."""
    for attempt in range(3):
        if slot_with("Claim All Coins") is not None:
            return
        if slot_with("Manage Orders") is None:
            menu_gap()
            ms.execute("/bz")
            tick(f"/bz sent (try {attempt})")
            try:
                wait_for("Manage Orders", timeout=2.5)
            except RuntimeError:
                tick("bz menu never appeared")
                ensure_clean()
                time.sleep(0.4)
                continue
            tick("bz menu up")
        press_name("Manage Orders")
        try:
            wait_for("Claim All Coins", timeout=1.5)
            tick("orders menu up")
            return
        except RuntimeError:
            tick("orders menu never appeared")
            time.sleep(0.2)   # the click was dropped or the menu is slow: look again, click again
    ensure_clean()
    if where() != "skyblock":
        _rejoin_hint[0] = time.time()   # the watchdog goes back to SkyBlock
        raise RuntimeError("the bazaar is not available: the character is not in SkyBlock (limbo or a lobby); it is being sent back")
    open_product(product or "COAL")
    press_name("Manage Orders")
    wait_for("Claim All Coins", timeout=4)


_me = {"name": None}


def my_name():
    """This account's player name (cached)."""
    if _me["name"] is None:
        try:
            _me["name"] = str(ms.player_name() or "")
        except Exception:
            _me["name"] = ""
    return _me["name"]


def parse_orders():
    rows = []
    for it in ms.container_get_items() or []:
        m = re.match(r"^(BUY|SELL)\s+(.+)$", item_name(it))
        if not m:
            continue
        lore = item_lore(it)
        amt = re.search(r"(?:Offer|Order) amount:\s*([\d,]+)", lore)
        px = re.search(r"Price per unit:\s*([\d,\.]+)", lore)
        # Hypixel abbreviates large totals ("205 /2.5k"), so accept k/M/B suffixes
        fl = re.search(r"Filled:\s*([\d,\.]+\s*[kKmMbB]?)\s*/\s*([\d,\.]+\s*[kKmMbB]?)", lore)
        # Hypixel abbreviates big counts ("You have 71.7k items to claim!"), so accept decimals and k/M/B suffixes
        tc = re.search(r"You have\s*([\d,\.]+\s*[kKmMbB]?)\s*items?\s*to claim", lore, re.I)
        rows.append({
            "slot": it.slot, "side": m.group(1), "product": m.group(2),
            "amount": int(num(amt.group(1))) if amt else None,
            "price": num(px.group(1)) if px else None,
            "filled": int(parse_amount(fl.group(1))) if fl else 0,
            "total": int(parse_amount(fl.group(2))) if fl else (int(num(amt.group(1))) if amt else None),
            "claimable": "to claim" in lore.lower(),
            "to_claim": int(parse_amount(tc.group(1))) if tc else 0,
            "claim_text": (re.search(r"You have[^!]*claim[^!]*!", lore) or [""])[0] if "to claim" in lore.lower() else "",
            "lore": lore[:200],
            # A co-op shares one order list: every order says who placed it ("By: [VIP] PlayerName"). Only our own may be claimed, cancelled or flipped.
            "owner": (re.search(r"By:\s*(?:\[[^\]]*\]\s*)?([A-Za-z0-9_]{1,16})", lore) or [None, None])[1],
        })
        rows[-1]["mine"] = rows[-1]["owner"] is None or not my_name() or rows[-1]["owner"].lower() == my_name().lower()
    return rows


def list_orders(product=None):
    open_orders(product)
    return parse_orders()


def find_order(rows, product, side, price=None, prefer=None):
    """First order matching product/side/price. An iceberg keeps an old, filled order waiting to be claimed next to its
    new, still-open one, often at the same price, so callers say which they mean: prefer="claim" picks one with goods
    to claim, prefer="open" picks one that is not completely filled."""
    hits = [r for r in rows if r.get("mine", True) and r["side"] == side.upper() and r["product"].lower() == product.lower()
            and (price is None or r["price"] is None or abs(r["price"] - float(price)) < 0.05)]
    if prefer == "claim":
        hits.sort(key=lambda r: not r["claimable"])
    elif prefer == "open":
        hits.sort(key=lambda r: bool(r["total"]) and r["filled"] >= r["total"])
    return hits[0] if hits else None


def cancel_order(product, side, price=None):
    """Cancel one order. An order with something already filled hands those goods / coins over when it is clicked and opens no cancel
    menu: then look at the menu again and click once more (the order is still there, now without the filled part)."""
    open_orders()
    r = find_order(parse_orders(), product, side, price, prefer="open")
    if not r:
        raise RuntimeError(f"no open {side} order for {product} found")
    for attempt in range(2):
        t0 = time.time()
        click_slot(r["slot"])
        try:
            press_name("Cancel Order")
            break
        except RuntimeError:
            if attempt == 1:
                raise
            ensure_clean()
            open_orders()
            r2 = find_order(parse_orders(), product, side, price, prefer="open")
            if r2 is None:
                return f"the {side} order for {product} is gone: it had filled and was handed over"
            if r2["total"] and r2["filled"] >= r2["total"]:
                return f"the {side} order for {product} had filled completely: the filled part was handed over (nothing left to cancel)"
            r = r2
    msg = expect_chat(t0, ["Cancelled"], timeout=5)
    if not msg:
        raise RuntimeError("no '[Bazaar] Cancelled' confirmation")
    return msg


def cancel_orders(items):
    """Cancel several orders with the Manage Orders menu kept open between them (a single cancel_order reopens /bz every time).
    items: [{product, side, price}]. Returns (cancelled, notes)."""
    done, notes = 0, []
    open_orders()
    for it in items:
        product, side, price = it["product"], it["side"], it.get("price")
        for attempt in range(2):
            if slot_with("Claim All Coins") is None:
                open_orders()
            r = find_order(parse_orders(), product, side, price, prefer="open")
            if r is None or (r["total"] and r["filled"] >= r["total"]):
                notes.append(f"{side} {product}: nothing left to cancel")
                break
            t0 = time.time()
            click_slot(r["slot"])
            try:
                press_name("Cancel Order")
            except RuntimeError:
                continue   # a filled part was handed over instead of a cancel menu: look at the list again and click once more
            if expect_chat(t0, ["Cancelled"], timeout=5):
                done += 1
            else:
                notes.append(f"{side} {product}: no 'Cancelled' confirmation")
            break
        else:
            notes.append(f"{side} {product}: could not cancel")
    return done, notes


def claim_order(product_id, product_name, side, price=None, max_rounds=60, interval=None, max_clicks=3000, stall_clicks=16, sack_on_full=False, npc_on_full=False):
    """Claim a filled order *completely*. One click only pulls what fits in the inventory (e.g. 35 free
    slots x 64 = 2,240); the compactor and sacks then free space, so click again until the escrow is empty.
    Stops if two clicks in a row move nothing (inventory full of something that can't be compacted/sacked).
    Returns (last chat line, signals, details). Signals are measured over the whole claim, in chain units."""
    inv0, pu0 = chain_inv(product_id), purse()
    ensure_clean()
    t_claim = time.time()
    msgs, rounds, stalled, last, reason = [], 0, 0, None, "done"
    first_remaining = None
    if interval:
        # STREAM MODE: open the Manage Orders menu once, then click the order at a flat interval (e.g. 0.5 s).
        # The menu stays open after a partial claim, and the order's own "You have N items to claim" line (read from the
        # same open menu) shows progress, so there is no closing/reopening between clicks.
        open_orders(product_id)
        clicks, same, last_val = 0, 0, None
        cid = None   # the menu's window id, read once and reused for the clicks (re-read if progress stalls)
        next_t = time.time()
        progress_t, sack_runs = time.time(), 0
        npc_sold, npc_coins, npc_runs = 0, 0.0, 0   # when the escrow last went down; how often we emptied the inventory into the sacks
        while clicks < max_clicks:
            if not menu_open() or slot_with("Claim All Coins") is None:
                open_orders(product_id)  # the menu closed under us: reopen and carry on
                cid = None
            r = find_order(parse_orders(), product_name, side, price, prefer="claim")
            if r is None and clicks:
                # the menu can be mid-redraw right after a claim: look again (reopened) before believing the order is gone
                for _ in range(3):
                    time.sleep(0.6); open_orders(product_id); cid = None
                    r = find_order(parse_orders(), product_name, side, price, prefer="claim")
                    if r is not None: break
            if r is None:
                reason = "order fully claimed" if clicks else f"no {side} order for {product_name}"
                break
            if not r["claimable"] or not r["to_claim"]:
                reason = "nothing left to claim"
                if not clicks:
                    raise RuntimeError(f"{side} order for {product_name} has nothing to claim yet ({r['filled']}/{r['total']} filled)")
                break
            if first_remaining is None:
                first_remaining = r["to_claim"]
            if last_val is not None and r["to_claim"] < last_val: progress_t = time.time()
            same = same + 1 if (last_val is not None and r["to_claim"] >= last_val) else 0
            last_val = last = r["to_claim"]
            # The game says so the moment the inventory is full: no need to keep clicking for 16 rounds to find out.
            full_hits = sum(1 for ts, t in list(_chat) if ts >= progress_t and "have the space required" in t)
            got_some = any(ts >= progress_t and "Claimed" in t for ts, t in list(_chat))
            if full_hits >= (1 if got_some else 2):   # it just handed over what fit, then said "no space": empty the inventory now
                if npc_on_full and npc_runs < 200:
                    # Sell what was just claimed to the NPC straight away, then carry on claiming: the goods spend seconds in the inventory, not minutes in the
                    # sacks (which fill up and spill into the stash).
                    ensure_clean()
                    try:
                        sold_now, gained_now, _ = trades_sell(product_id)
                    except RuntimeError:
                        sold_now, gained_now = 0, None
                    npc_runs += 1
                    if sold_now > 0:
                        npc_sold += sold_now; npc_coins += gained_now or 0
                        same, last_val, progress_t, cid = 0, None, time.time(), None
                        continue
                if sack_on_full and sack_runs < 80:
                    ensure_clean()
                    moved = insert_into_sacks()
                    sack_runs += 1
                    if moved > 0:
                        same, last_val, progress_t, cid = 0, None, time.time(), None   # room again: carry on claiming
                        continue
                    reason = f"stalled with {r['to_claim']:,} still in escrow (inventory full and no sack takes what is in it)"
                else:
                    reason = f"stalled with {r['to_claim']:,} still in escrow (inventory full)"
                break
            if same >= stall_clicks:
                reason = f"stalled with {r['to_claim']:,} still in escrow (inventory full?)"
                break
            if cid is None or same >= 2:
                cid = menu_id()
            click_slot(r["slot"], wait=False, cid=cid)
            clicks += 1
            next_t += jit(interval)  # flat schedule (+-5% random so it never locks into a rhythm that gets ignored)
            time.sleep(max(0.0, next_t - time.time()))
        rounds = clicks
        msgs = [t for ts, t in list(_chat) if ts >= t_claim and "Claimed" in t]
    while rounds < max_rounds and not interval:
        open_orders(product_id)
        r = find_order(parse_orders(), product_name, side, price, prefer="claim")
        if r is None:
            reason = "order fully claimed" if rounds else f"no {side} order for {product_name}"
            break
        if not r["claimable"] or not r["to_claim"]:
            reason = "nothing left to claim"
            if not rounds:
                raise RuntimeError(f"{side} order for {product_name} has nothing to claim yet ({r['filled']}/{r['total']} filled)")
            break
        if first_remaining is None:
            first_remaining = r["to_claim"]
        if last is not None and r["to_claim"] >= last:
            stalled += 1
            if stalled >= 2:
                reason = f"stalled with {r['to_claim']:,} still in escrow (inventory full?)"
                break
        else:
            stalled = 0
        last = r["to_claim"]
        t0 = time.time()
        click_slot(r["slot"])
        m = expect_chat(t0, ["Claimed"], timeout=5)
        if m:
            msgs.append(m)
        ensure_clean()
        settle_inventory(0.35, 0.2)  # let the compactor merge and the sacks absorb it before the next pull
        time.sleep(jit(0.02))
        rounds += 1
    ensure_clean()
    settle_inventory(0.5, 0.2)
    inv1, pu1 = chain_inv(product_id), purse()
    buy = side.upper() == "BUY"
    sig = {
        "chat": bool(msgs),
        "orders": rounds > 0,
        "sacks": None,  # arrives later as a "signals" message (the [Sacks] chat line is batched over 5 s)
        "inventory": (inv1 > inv0) if buy else None,
        "purse": (pu1 is not None and pu0 is not None and pu1 > pu0) if not buy else None,
    }
    info = {"rounds": rounds, "reason": reason, "claimed_msgs": len(msgs), "t_claim": t_claim, "buy": buy,
            "escrow_before": first_remaining, "escrow_left": last if reason.startswith("stalled") else 0, "sack_runs": locals().get("sack_runs", 0),
            "npc_sold": locals().get("npc_sold", 0), "npc_coins": locals().get("npc_coins", 0.0)}
    # how many units the whole claim moved: the game says "Claimed Nx <item>" per click, and one claim can run through several orders at the same price
    try:
        nm = re.escape(product_name)
        info["claimed_units"] = sum(int(m.group(1).replace(",", "")) for ts, t in list(_syslog) if ts >= t_claim for m in [re.search(r"Claimed ([\d,]+)x " + nm, t)] if m)
    except Exception:
        pass
    return (msgs[-1] if msgs else ""), sig, info


def flip_order(product_id, side, price, sell_price):
    """Flip a filled order straight from escrow into a new sell offer (no inventory involved, so size is unlimited).
    Verified flow: right-click the order -> 'Flip Order' -> type the sell price into the sign."""
    open_orders(product_id)
    # a completely filled slice first, not the live (maybe part-filled) order next to it at the same price
    hits = [x for x in parse_orders() if x.get("mine", True) and x["side"] == side.upper() and x["product"].lower() == display(product_id).lower()
            and (price is None or x["price"] is None or abs(x["price"] - float(price)) < 0.05)]
    hits.sort(key=lambda x: (not (x["claimable"] and x["total"] and x["filled"] >= x["total"]), not x["claimable"]))
    r = hits[0] if hits else None
    if r is None:
        raise RuntimeError(f"no {side} order for {display(product_id)} found")
    if not r["claimable"]:
        raise RuntimeError(f"nothing filled yet to flip ({r['filled']}/{r['total']})")
    qty = r["to_claim"]
    t0 = time.time()
    click_slot(r["slot"], 1)  # right-click opens the order's options
    try:
        press_name("Flip Order")
    except RuntimeError as e:
        if "never appeared" not in str(e):
            raise
        # Hypixel only offers Flip Order on some orders (e.g. not on a part-filled live one): not a failure, the filled goods get claimed instead
        ensure_clean()
        raise RuntimeError(f"nothing filled yet to flip (no Flip Order button, {r['filled']}/{r['total']} filled)")
    type_in_sign(fmt_price(sell_price))
    msg = expect_chat(t0, ["Order Flipped"], timeout=6)
    if not msg:
        ensure_clean()
        raise RuntimeError("no '[Bazaar] Order Flipped' confirmation")
    return msg, qty


def wait_purse_change(before, timeout=4.0):
    """Poll the scoreboard until the purse differs from `before` (it lags the game's confirmation by up to a couple of seconds)."""
    end = time.time() + timeout
    while time.time() < end:
        p = purse()
        if p is not None and before is not None and abs(p - before) >= 1:
            return p
        time.sleep(0.1)
    return purse()


def bank_op(account, action, amount=None, keep=None, target=None):
    """Deposit into / withdraw from a bank account through /bank (needs the Booster Cookie buff).
    account: "coop" or "personal". action: "deposit" or "withdraw".
    amount: a number of coins, or "all" / "half" (withdraw also "20%"). deposit keep=N: deposit everything except N coins.
    withdraw target=N: withdraw enough to have N coins in the purse. Returns the game's confirmation line."""
    account = "personal" if str(account).lower().startswith("p") else "coop"
    action = "withdraw" if str(action).lower().startswith("w") else "deposit"
    before = purse() or 0.0
    if action == "deposit" and keep is not None:
        amount = int(before - float(keep))
        if amount <= 0:
            raise RuntimeError(f"purse is {before:,.0f}: nothing above {float(keep):,.0f} to deposit")
    if action == "withdraw" and target is not None:
        amount = int(float(target) - before)
        if amount <= 0:
            raise RuntimeError(f"purse already has {before:,.0f} (target {float(target):,.0f})")
    if isinstance(amount, str):
        amount = amount.lower().replace(" ", "")
    ensure_clean()
    t0 = time.time()
    menu_gap()
    ms.execute("/bank")
    deadline = time.time() + 5
    while time.time() < deadline and slot_with("Co-op Bank Account") is None:
        if any(ts >= t0 and "Cookie Buff" in t for ts, t in list(_syslog)):
            raise RuntimeError("/bank needs the Booster Cookie buff on this account (or the banker NPC in the hub)")
        lim = next((t for ts, t in list(_syslog) if ts >= t0 and "You can only open" in t), None)
        if lim:
            raise RuntimeError(lim)
        time.sleep(0.1)
    if slot_with("Co-op Bank Account") is None:
        raise RuntimeError("the bank menu did not open")
    t_acc = time.time()
    press_name("Co-op Bank Account" if account == "coop" else "Personal Bank Account")
    try:
        press_name("Deposit Coins" if action == "deposit" else "Withdraw Coins")
    except RuntimeError:
        lim = expect_syslog(t_acc, ["You can only", "cannot"], timeout=1.0)
        raise RuntimeError(lim or "the account menu did not open")
    if action == "deposit":
        names = {"all": "Your whole purse", "half": "Half your purse"}
    else:
        names = {"all": "Everything in the account", "half": "Half the account", "20%": "Withdraw 20%"}
    t1 = time.time()
    if isinstance(amount, str) and amount in names:
        press_name(names[amount])
    else:
        try:
            coins = int(float(amount))
        except Exception:
            raise RuntimeError(f"bad amount: {amount!r}")
        if coins <= 0:
            raise RuntimeError("amount must be above 0")
        press_name("Specific amount")
        type_in_sign(str(coins))
    msg = expect_syslog(t1, ["Deposited", "Withdrew", "cannot", "not enough", "You can only", "Invalid"], timeout=8)
    if msg and any(k in msg for k in ("Deposited", "Withdrew")):
        wait_purse_change(before)   # the scoreboard shows the new purse a moment after the chat line: do not report the old one
        return msg
    if msg:
        raise RuntimeError(msg)
    # no line seen: believe the purse
    time.sleep(1.0)
    after = purse()
    if after is not None and abs(after - before) >= 1:
        return f"{'Deposited' if action == 'deposit' else 'Withdrew'} (purse {before:,.0f} -> {after:,.0f})"
    raise RuntimeError("no bank confirmation and the purse did not change")


def claim_coins():
    open_orders()
    t0 = time.time()
    slot = slot_with("Claim All Coins")
    lore = next((item_lore(it) for it in ms.container_get_items() or [] if it.slot == slot), "")
    if "don't have any" in lore:
        return "no coins to claim"
    click_slot(slot)
    return expect_chat(t0, ["Claimed"], timeout=4) or "claim sent"


def read_bank(force=False):
    """Exact bank balance from /bank ('Current balance' on the Deposit Coins button)."""
    if not force and _bank["value"] is not None and time.time() - _bank["at"] < BANK_TTL:
        return _bank["value"]
    menu_gap()
    ms.execute("/bank")
    deadline = time.time() + 4
    val = None
    while time.time() < deadline and val is None:
        for it in ms.container_get_items() or []:
            if item_name(it) == "Deposit Coins":
                m = re.search(r"Current balance:\s*([\d,\.]+)", item_lore(it))
                if m:
                    val = num(m.group(1))
        time.sleep(0.15)
    if val is not None:
        _bank["value"], _bank["at"] = val, time.time()
    return val if val is not None else _bank["value"]


def item_product_id(it):
    """SkyBlock id of an inventory stack: its custom_data id, else the product whose vanilla item it is."""
    nbt = str(it.nbt) if it.nbt else ""
    m = re.search(r'id:"([A-Za-z0-9_\-:]+)"', nbt)
    if m:
        return m.group(1).upper()
    for pid, vanilla in VANILLA.items():
        if vanilla == it.item:
            return pid
    return it.item.split(":")[-1].upper()


def inv_all() -> dict:
    """Everything in the 36 storage slots, summed per product id, with how many more of each would still fit."""
    counts, room = {}, {}
    used = set()
    slots = []   # every occupied storage slot, for the app's picture of the inventory (0-8 hotbar, 9-35 the rest)
    for it in inventory_items():
        if it.slot is None or not 0 <= it.slot <= 35:
            continue
        used.add(it.slot)
        pid = item_product_id(it)
        slots.append({"slot": it.slot, "id": pid, "count": it.count})
        counts[pid] = counts.get(pid, 0) + it.count
        room[pid] = room.get(pid, 0) + max(0, STACK - it.count)
    free = 36 - len(used)
    items = [{"id": pid, "count": n, "fits": free * STACK + room[pid]} for pid, n in counts.items()]
    items.sort(key=lambda r: -r["count"])
    return {"type": "invall", "items": items, "free_slots": free, "slots": slots}


def inv_report(pid: str) -> dict:
    """How many of `pid` are in the inventory (what an insta-sell / sell offer would sell) and how many more
    would fit (free slots plus room left in that item's part-full stacks)."""
    return {"type": "invcount", "product": pid, "count": inv_count(pid), "fits": inv_room(pid)}


# ---------- command handling ----------


# ---------------------------------------------------------------- live order book ("quicker orderbook updates")
# A spare account reads the top 7 buy orders and sell offers straight off each product's bazaar page, over and over, and
# pushes them to every connected terminal. Hypixel's API only refreshes every ~20 s; this is a few seconds old at most.
_book = {"products": [], "thread": None, "gen": 0}
_BOOK_RE = re.compile(r"([\d,]+(?:\.\d+)?) coins\s+each \|\s+([\d,.]+[kKmM]?)\s*x\s+(?:in|from)\s+([\d,]+)\s+(?:orders?|offers?)")


def _num(t):
    t = t.replace(",", "").strip()
    mul = 1
    if t[-1:] in "kK": mul, t = 1_000, t[:-1]
    elif t[-1:] in "mM": mul, t = 1_000_000, t[:-1]
    return float(t) * mul


def read_book(product_id):
    """Open a product's page and read its top orders/offers: ([(price, amount, orders)] bids, [...] asks)."""
    t0 = time.time()
    try:
        open_product(product_id)
    except RuntimeError:
        if any(ts >= t0 and "Cookie Buff" in text for ts, text in list(_syslog)):
            raise RuntimeError("NO_COOKIE: this account has no Booster Cookie, so it cannot open the bazaar from anywhere")
        raise
    bids, asks = [], []
    for it in ms.container_get_items() or []:
        if it.slot not in (15, 16):
            continue
        lv = [(_num(a), int(_num(b)), int(_num(c))) for a, b, c in _BOOK_RE.findall(item_lore(it))]
        if it.slot == 15: bids = lv
        else: asks = lv
    return bids, asks


def _page_book():
    """Read the order book off the product page that is open now, or None if it is not a product page."""
    bids = asks = None
    for it in ms.container_get_items() or []:
        if it.slot == 15 and item_name(it) == "Create Buy Order":
            bids = [(_num(a), int(_num(b)), int(_num(c))) for a, b, c in _BOOK_RE.findall(item_lore(it))]
        elif it.slot == 16 and item_name(it) == "Create Sell Offer":
            asks = [(_num(a), int(_num(b)), int(_num(c))) for a, b, c in _BOOK_RE.findall(item_lore(it))]
    return None if bids is None or asks is None else (bids, asks)


def _book_loop(gen):
    i = 0
    while _book["gen"] == gen:
        focus = _book.get("focus")
        prods = list(_book["products"])
        if not prods and not focus:
            break
        try:
            if focus:
                # one item, every second: stay on its page and read it again and again; reopen it now and then (and
                # whenever something else used the game) so the page cannot go stale
                with _lock:
                    # Hypixel does not refresh an open page (tested: numbers only change when it is reopened), so every read reopens it
                    read_book(focus)
                    got = _page_book()
                if got and (got[0] or got[1]):
                    push({"type": "book", "product": focus, "bids": got[0], "asks": got[1], "t": time.time()})
                time.sleep(0.05)
                continue
            pid = prods[i % len(prods)]
            i += 1
            with _lock:
                bids, asks = read_book(pid)
                ensure_clean()
            if bids or asks:
                push({"type": "book", "product": pid, "bids": bids, "asks": asks, "t": time.time()})
        except Exception as e:
            push({"type": "book_err", "product": focus or "", "msg": str(e)[:200]})
            time.sleep(300 if "NO_COOKIE" in str(e) else 3)   # no cookie: stop hammering /bz, try again in 5 minutes
            continue
        time.sleep(0.3)   # leaves a gap for anything else this agent is asked to do
    if _book["gen"] == gen:
        _book["thread"] = None
        try:
            with _lock:
                ensure_clean()
        except Exception:
            pass


def handle(conn, line):
    def reply(obj):
        if obj.get("type") in ("fill", "placed", "claimed", "flipped", "batch", "bulksell", "orders", "requoted", "requoted_sell"):
            obj["timing"] = timing()
        send(conn, obj)

    try:
        msg = json.loads(line)
        cmd = msg.get("cmd")
        begin_timing()
        if cmd == "balance":
            if msg.get("bank") == "now":   # only an explicit "Refresh balance + bank" opens the bank menu; a plain true never does
                with action():
                    read_bank(force=True)
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd in ("instabuy", "instasell"):
            pid = msg["product"]
            buy = cmd == "instabuy"
            mode = msg.get("sack_verify", SACK_VERIFY)
            with action():
                if msg.get("from_sacks") and not buy:
                    pull_from_sacks(pid, msg.get("qty"))
                before, inv0 = purse(), chain_inv(pid)
                sk0 = chain_sacks(pid) if mode == "menu" else None
                ensure_clean()
                t_start = time.time()
                text = (instabuy_rounds if buy else instasell)(pid, msg["qty"])
                ensure_clean()
                if msg.get("fast"):
                    # Report the fill the moment Hypixel confirms it; the purse / inventory checks follow
                    # as a "signals" message (the app merges them into the trade's verification).
                    reply({"type": "fill", "msg": text, "signals": {"chat": True}, "fast": True})
                after, inv1 = wait_purse(before), chain_inv(pid)
                inv_ok = (inv1 > inv0) if buy else (inv1 < inv0)
                sacks_sig, late = None, False
                if mode == "menu":
                    sk1 = chain_sacks(pid)
                    sacks_sig = None if sk0 is None or sk1 is None else ((sk1 > sk0) if buy else (sk1 < sk0))
                elif not inv_ok:
                    late = True  # goods went straight into sacks: confirm from the [Sacks] line, after replying
            sig = {"chat": True,
                   "purse": None if before is None or after is None else ((after < before) if buy else (after > before)),
                   "inventory": inv_ok,
                   "sacks": sacks_sig}
            out = {"type": "fill", "msg": text, "signals": sig}
            if buy and before is not None and after is not None and before > after:
                out["cost"] = before - after
            if msg.get("fast"):
                reply({"type": "signals", "product": pid, "side": "BUY" if buy else "SELL", "signals": sig,
                       "cost": out.get("cost"), "msg": "verified: " + ", ".join(k for k, v in sig.items() if v)})
            else:
                reply(out)
            reply({"type": "balance", "purse": after, "bank": _bank["value"]})
            reply(inv_report(pid))
            if late:
                threading.Thread(target=late_sack_signal, args=(conn, pid, "BUY" if buy else "SELL", t_start, buy), daemon=True).start()
        elif cmd in ("buyorder", "selloffer"):
            side = "BUY" if cmd == "buyorder" else "SELL"
            row = None
            with action():
                if cmd == "buyorder":
                    _last_buy_unit[0] = None
                    text = buyorder(msg["product"], msg.get("qty", 1), msg.get("price"), top_plus=bool(msg.get("top_plus")), max_price=msg.get("max_price"))   # top_plus false + price = join a co-op partner's bid
                else:
                    if msg.get("from_sacks"):
                        pull_from_sacks(msg["product"], msg.get("qty"))
                    text = selloffer(msg["product"], msg.get("qty", 1), msg["price"])
                unit = _last_buy_unit[0] if cmd == "buyorder" else None
                if msg.get("check", True):  # look at the order's real state (it may have filled instantly)
                    time.sleep(1.0)
                    row = find_order(list_orders(msg["product"]), display(msg["product"]), side, unit or msg["price"])
            reply({"type": "placed", "msg": text, "order": row, "price": unit})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "requote":
            # One in-game action: open the order menu once, cancel what is left of the resting buy order, and place the
            # next one at the top bid + 0.1. Replaces three separate commands (orders, cancel, buyorder).
            pid = msg["product"]
            name = display(pid)
            with action():
                r = find_order(list_orders(pid), name, "BUY", msg.get("price"), prefer="open")
                if r is None:
                    raise RuntimeError(f"no open BUY order for {name} found")
                if r["total"] and r["filled"] >= r["total"]:
                    reply({"type": "requoted", "product": pid, "result": "filled", "filled": r["filled"], "total": r["total"]})
                else:
                    t0 = time.time()
                    click_slot(r["slot"])
                    try:
                        press_name("Cancel Order")
                    except RuntimeError:
                        # An order with something filled hands the goods over when clicked and opens no cancel menu: look again at the real menu
                        # and click again. Up to three times: with a lot of fills it can take more than one click to empty it.
                        done_retry = False
                        for _attempt in range(3):
                            ensure_clean()
                            r2 = find_order(list_orders(pid), name, "BUY", msg.get("price"), prefer="open")
                            if r2 is None:
                                raise
                            if r2["total"] and r2["filled"] >= r2["total"]:
                                reply({"type": "requoted", "product": pid, "result": "filled", "filled": r2["filled"], "total": r2["total"]})
                                r = None; done_retry = True
                                break
                            r = r2; t0 = time.time()
                            click_slot(r["slot"])
                            try:
                                press_name("Cancel Order")
                                done_retry = True
                                break
                            except RuntimeError:
                                continue
                        if not done_retry:
                            raise RuntimeError("expected 'Cancel Order' in the open menu but it never appeared")
                    cm = expect_chat(t0, ["Cancelled"], timeout=5) if r is not None else None
                    if r is not None and not cm:
                        raise RuntimeError("no '[Bazaar] Cancelled' confirmation")
                    if r is not None:
                        qty = min(int(msg.get("slice", 71680)), int(msg.get("remaining", 0)) - r["filled"], int(msg.get("max_qty", 71680)), 71680)
                        out = {"type": "requoted", "product": pid, "result": "cancelled", "filled": r["filled"], "total": r["total"], "cancel_msg": cm, "qty": max(qty, 0)}
                        if qty >= 1:
                            try:
                                ensure_clean()
                                _last_buy_unit[0] = None
                                if msg.get("join"):   # the top bid is our co-op partner's: join its price instead of bidding 0.1 over it
                                    out["placed"] = buyorder(pid, qty, msg["join"], top_plus=False, max_price=msg.get("max_price")); _last_buy_unit[0] = float(msg["join"])
                                else:
                                    out["placed"] = buyorder(pid, qty, msg.get("price"), top_plus=True, max_price=msg.get("max_price"))
                                out["price"] = _last_buy_unit[0]
                                # the new order is now the top one: cancel any of our other, lower BUY orders for this item
                                if msg.get("cancel_below", True) and out["price"]:
                                    out["cancelled_below"] = 0
                                    try:
                                        others = [o for o in list_orders(pid) if o.get("mine", True) and o["side"] == "BUY" and o["product"].lower() == name.lower()
                                                  and o["price"] is not None and o["price"] < float(out["price"]) - 0.05
                                                  and not (o["total"] and o["filled"] >= o["total"])][:8]
                                        if others:
                                            # one at a time through the batch cancel: an order with something filled hands the goods over when clicked and needs a second click
                                            n, notes = cancel_orders([{"product": name, "side": "BUY", "price": o["price"]} for o in others])
                                            out["cancelled_below"] = n
                                            if notes: out["cancel_below_error"] = "; ".join(notes)
                                    except Exception as e2:
                                        out["cancel_below_error"] = str(e2)
                            except Exception as e:   # the cancel already happened: report it, so the app books the partial fill
                                out["place_error"] = str(e)
                        reply(out)
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "requote_sell" and msg.get("all"):
            # Auto Flip: merge. Cancel every open SELL offer of this item that fits in the inventory, then list it all as one offer
            # at `new_price` (a sell offer always takes the whole stack). Frees order slots; one offer re-lists in one action after.
            pid = msg["product"]
            name = display(pid)
            with action():
                rows = [r for r in list_orders(pid) if r.get("mine", True) and r["side"] == "SELL" and r["product"].lower() == name.lower()
                        and not (r["total"] and r["filled"] >= r["total"])]
                room = free_slots() * 64 - 64   # keep a slot spare
                items, want = [], 0
                for r in sorted(rows, key=lambda r: (r["total"] or 0) - (r["filled"] or 0)):
                    left = max((r["total"] or 0) - (r["filled"] or 0), 0)
                    if want + left > room:
                        break
                    items.append({"product": name, "side": "SELL", "price": r.get("price")}); want += left
                if len(items) < 2:
                    raise RuntimeError("nothing to merge (fewer than two sell offers fit in the inventory)")
                inv0 = inv_count(pid)
                n, notes = cancel_orders(items)
                ensure_clean()
                settle_inventory(1.5, 0.3)
                got = inv_count(pid) - inv0
                if got < want:
                    try:
                        sack_get(pid, want - got)
                    except Exception:
                        pass
                    got = inv_count(pid) - inv0
                out = {"type": "requoted_sell", "product": pid, "result": "cancelled", "merged": n, "remaining": want, "returned": got,
                       "price": msg.get("new_price"), "notes": notes}
                if inv_count(pid) >= 1:
                    try:
                        ensure_clean()
                        out["placed"] = selloffer(pid, inv_count(pid), msg.get("new_price"))
                    except Exception as e:
                        out["place_error"] = str(e)
                else:
                    out["place_error"] = "nothing came back to the inventory"
            reply(out)
        elif cmd == "requote_sell":
            # Auto Flip: re-list an undercut SELL offer. Open the order menu once, cancel the offer (the goods come back to the
            # inventory), then list everything of that item again at `new_price`. A sell offer always lists the whole stack.
            pid = msg["product"]
            name = display(pid)
            with action():
                r = find_order(list_orders(pid), name, "SELL", msg.get("price"), prefer="open")
                if r is None:
                    raise RuntimeError(f"no open SELL order for {name} found")
                remaining = max((r["total"] or 0) - (r["filled"] or 0), 0)
                filled_out = None
                if r["total"] and r["filled"] >= r["total"]:
                    filled_out = r
                else:
                    inv0 = inv_count(pid)
                    t0 = time.time()
                    click_slot(r["slot"])
                    try:
                        press_name("Cancel Order")
                    except RuntimeError:
                        # an offer with something sold hands the coins over when clicked and opens no cancel menu: look again, click once more
                        ensure_clean()
                        r2 = find_order(list_orders(pid), name, "SELL", msg.get("price"), prefer="open")
                        if r2 is None or (r2["total"] and r2["filled"] >= r2["total"]):
                            filled_out = r2 or r
                        else:
                            r = r2
                            remaining = max((r["total"] or 0) - (r["filled"] or 0), 0)
                            inv0 = inv_count(pid)
                            t0 = time.time()
                            click_slot(r["slot"])
                            press_name("Cancel Order")
                if filled_out is not None:
                    reply({"type": "requoted_sell", "product": pid, "result": "filled", "filled": filled_out["filled"], "total": filled_out["total"]})
                else:
                    cm = expect_chat(t0, ["Cancelled"], timeout=5)
                    if not cm:
                        raise RuntimeError("no '[Bazaar] Cancelled' confirmation")
                    ensure_clean()
                    settle_inventory(1.5, 0.3)
                    got = inv_count(pid) - inv0
                    if got < remaining:   # the compactor / sacks may have taken some: pull the rest back
                        try:
                            sack_get(pid, remaining - got)
                        except Exception:
                            pass
                        got = inv_count(pid) - inv0
                    out = {"type": "requoted_sell", "product": pid, "result": "cancelled", "filled": r["filled"], "remaining": remaining,
                           "returned": got, "old_price": r.get("price"), "price": msg.get("new_price"), "cancel_msg": cm}
                    if got >= 1 or inv_count(pid) >= 1:
                        try:
                            ensure_clean()
                            out["placed"] = selloffer(pid, got, msg.get("new_price"))
                        except Exception as e:   # the cancel already happened: report it so the app knows the goods are in the inventory
                            out["place_error"] = str(e)
                    else:
                        out["place_error"] = "nothing came back to the inventory to list (stash full?)"
                    reply(out)
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "rejoin":
            with action():
                res = rejoin_skyblock()
            reply({"type": "rejoin", "msg": res})
        elif cmd == "subscribe_chat":
            _chat_subs.add(conn)
            reply({"type": "log", "msg": "chat feed on"})
        elif cmd == "say":
            # chat failures are reported as log lines, never as errors: the app treats "error" as a failed order
            try:
                text = str(msg.get("text", "")).strip()
                if not text:
                    raise RuntimeError("empty message")
                if len(text) > 256:
                    raise RuntimeError("message too long (256 max)")
                if text.startswith("\\"):
                    raise RuntimeError("Minescript commands (starting with a backslash) are not allowed from here")
                if time.time() - _say_at[0] < 1.0:
                    raise RuntimeError("sending too fast (1 message per second)")
                _say_at[0] = time.time()
                if text.startswith("/"):
                    ms.execute(text)   # a normal Hypixel command such as /msg or /pc
                else:
                    ms.chat(text)      # a chat message
                reply({"type": "log", "msg": f"sent: {text[:80]}"})
            except RuntimeError as e:
                reply({"type": "log", "msg": f"chat: {e}"})
        elif cmd == "batchorders":
            results, rows = [], []
            with action():
                for n, o in enumerate(msg["orders"]):
                    if n:  # let the previous confirm screen finish closing before opening the next product page
                        ensure_clean()
                        time.sleep(0.4)
                    fn = buyorder if o["side"].upper() == "BUY" else selloffer
                    try:
                        results.append({**o, "ok": True, "msg": fn(o["product"], o.get("qty", 1), o["price"])})
                    except Exception as e:
                        results.append({**o, "ok": False, "msg": str(e)})
                        break  # stop at the first failure rather than keep placing orders
                rows = list_orders(msg["orders"][0]["product"]) if msg["orders"] else []  # one state read for the batch
            reply({"type": "batch", "results": results, "orders": rows})
        elif cmd == "bulkpreview":
            with action():
                items, earn = bulk_preview(msg.get("what", "inventory"))
            reply({"type": "bulkpreview", "what": msg.get("what", "inventory"), "items": items, "earn": earn})
        elif cmd == "bulksell":
            with action():
                text, items, earn, gained = bulk_sell(msg.get("what", "inventory"), msg.get("allow", []))
            reply({"type": "bulksell", "msg": text, "items": items, "expected": earn, "gained": gained})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "sackget":
            t0 = time.time()
            with action():
                got = sack_get(msg["product"], msg["qty"])
            reply({"type": "log", "msg": f"moved {got}x {display(msg['product'])} from sacks to inventory"})
            threading.Thread(target=late_log, args=(reply, t0, "the pull"), daemon=True).start()
        elif cmd == "tradesell":
            with action():
                sold, gained, sig = trades_sell(msg["product"], msg.get("max_stacks"), msg.get("click_interval"))
            reply({"type": "fill", "msg": f"[Trades] sold {sold}x {display(msg['product'])} for {gained:,.1f} coins" if gained is not None else f"sold {sold}", "signals": {"chat": True, **sig}})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "tradesellqty":
            pid = msg["product"]
            with action():
                pu0 = purse()
                sold, gained, rounds = trades_sell_amount(pid, msg.get("qty"), msg.get("mode", "qty"), use_sacks=bool(msg.get("from_sacks", True)), interval=msg.get("click_interval"))
                pu1 = purse()
            reply({"type": "fill", "msg": f"[Trades] sold {sold:,}x {display(pid)}" + (f" for {gained:,.1f} coins" if gained is not None else "") + (f" ({rounds} rounds)" if rounds > 1 else ""),
                   "sold": sold, "gained": gained, "trades": True,
                   "signals": {"chat": True, "inventory": True, "purse": None if pu0 is None or pu1 is None else pu1 > pu0}})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
            reply(inv_report(pid))
        elif cmd == "pipeline":
            run_pipeline(msg["steps"], reply)
        elif cmd == "holdings":
            with action():
                h = holdings(msg["product"])
            reply({"type": "holdings", "product": msg["product"], **h})
        elif cmd == "orders":
            with action():
                rows = list_orders(msg.get("product"))
            reply({"type": "orders", "orders": rows})
        elif cmd == "claimorder":
            pid = msg["product"]
            with action():
                interval = float(msg.get("interval", 0.25)) if msg.get("mode", "stream") == "stream" else None
                text, sig, info = claim_order(pid, display(pid), msg["side"], msg.get("price"), interval=interval, sack_on_full=bool(msg.get("sack_on_full")), npc_on_full=bool(msg.get("npc_on_full")))
            reply({"type": "claimed", "msg": text, "product": pid, "side": msg["side"].upper(), "price": msg.get("price"), "signals": sig, "info": info})
            if info.get("buy") and info.get("rounds"):
                threading.Thread(target=late_sack_signal, args=(conn, pid, "BUY", info["t_claim"], True), daemon=True).start()
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "fliporder":
            with action():
                text, qty = flip_order(msg["product"], msg["side"], msg.get("price"), msg["sell_price"])
            reply({"type": "flipped", "msg": text, "product": msg["product"], "side": msg["side"].upper(),
                   "price": msg.get("price"), "sell_price": msg["sell_price"], "qty": qty})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "cancel" and msg.get("orders"):
            with action():
                n, notes = cancel_orders(msg["orders"])
            reply({"type": "log", "msg": f"cancelled {n}/{len(msg['orders'])} orders" + ("; " + "; ".join(notes) if notes else "")})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "cancel":
            with action():
                text = cancel_order(msg["product"], msg["side"], msg.get("price"))
            reply({"type": "log", "msg": text})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "bank":
            with action():
                text = bank_op(msg.get("account", "coop"), msg.get("action", "deposit"), msg.get("amount"), msg.get("keep"), msg.get("target"))
            mb = re.search(r"There's now ([\d.,]+\s*[kKmMbB]?) coins", text)
            reply({"type": "bank", "msg": text, "account": msg.get("account", "coop"), "action": msg.get("action", "deposit"),
                   "balance": int(parse_amount(mb.group(1))) if mb else None, "purse": purse()})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "use_slot":   # right-click the item in a hotbar slot (0-8), then show the screen that opens
            with _lock:
                ensure_clean()
                ms.player_inventory_select_slot(int(msg["slot"]))
                time.sleep(0.3)
                ms.player_press_use(True)
                time.sleep(0.15)
                ms.player_press_use(False)
                time.sleep(float(msg.get("wait", 1.5)))
            reply({"type": "dump", "screen": screen_class(), "items": dump_items(lore=True)})
        elif cmd == "claim":
            with action():
                text = claim_coins()
            reply({"type": "log", "msg": text})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "run":  # run a chat command and report the menu it opens (exploration helper)
            with _lock:
                ensure_clean()
                ms.execute(msg["command"])
                time.sleep(float(msg.get("wait", 1.5)))
            reply({"type": "dump", "screen": screen_class(), "items": dump_items(lore=True)})
        elif cmd == "reload":
            reply({"type": "log", "msg": "agent restarting (loads the newest installed version)…"})
            threading.Thread(target=reload_agent, daemon=True).start()
        elif cmd == "version":
            reply({"type": "version", "version": AGENT_VERSION})
        elif cmd == "benchgap":  # profiling helper: how soon after closing a menu can /bz be sent again and still open?
            res = []
            with action():
                for gap in (0.05, 0.15, 0.3, 0.5, 0.8, 1.2):
                    ok = 0; times = []
                    for _ in range(6):
                        t = time.time(); ms.execute("/bz")
                        got = False
                        while time.time() - t < 3:
                            if slot_with("Manage Orders") is not None:
                                got = True; break
                            time.sleep(0.02)
                        if got:
                            ok += 1; times.append(round(time.time() - t, 2))
                        ensure_clean(); time.sleep(gap)
                    res.append({"gap_after_close_s": gap, "ok": ok, "of": 6, "open_times": times})
            reply({"type": "benchgap", "results": res})
        elif cmd == "benchclick":  # profiling helper: how long after a menu appears does a click on it get through?
            res = []
            with action():
                for delay in (0.0, 0.08, 0.15, 0.2, 0.25, 0.3, 0.4):
                    ok = 0; times = []
                    for _ in range(6):
                        ms.execute("/bz")
                        t0 = time.time()
                        while time.time() - t0 < 3 and slot_with("Manage Orders") is None:
                            time.sleep(0.01)
                        time.sleep(delay)
                        slot = slot_with("Manage Orders")
                        if slot is not None:
                            t = time.time(); click_slot(slot)
                            while time.time() - t < 1.6 and slot_with("Claim All Coins") is None:
                                time.sleep(0.02)
                            if slot_with("Claim All Coins") is not None:
                                ok += 1; times.append(round(time.time() - t, 2))
                        ensure_clean(); time.sleep(0.2)
                    res.append({"delay_after_menu_s": delay, "ok": ok, "of": 6, "click_to_menu_s": times})
            reply({"type": "benchclick", "results": res})
        elif cmd == "benchclick2":  # profiling helper: does the gap since the last menu closed decide whether the next click works?
            res = []
            with action():
                for gap in (0.0, 0.1, 0.25, 0.5, 1.0):
                    ok = 0; first = 0
                    for _ in range(8):
                        ensure_clean(); time.sleep(gap)
                        ms.execute("/bz")
                        t0 = time.time()
                        while time.time() - t0 < 3 and slot_with("Manage Orders") is None:
                            time.sleep(0.01)
                        slot = slot_with("Manage Orders")
                        if slot is None:
                            continue
                        t = time.time(); click_slot(slot)
                        while time.time() - t < 1.0 and slot_with("Claim All Coins") is None:
                            time.sleep(0.02)
                        if slot_with("Claim All Coins") is not None:
                            ok += 1
                    ensure_clean()
                    res.append({"gap_after_close_s": gap, "click_ok": ok, "of": 8})
            reply({"type": "benchclick2", "results": res})
        elif cmd == "benchsign4":  # profiling helper: does the fast helper's sign op give the same result as the Java route?
            out = []
            with action():
                for route in ("fast", "java"):
                    open_product("SEEDS")
                    press_name("Create Buy Order")
                    press_name("Custom Amount")
                    end = time.time() + 3
                    while "Sign" not in screen_class() and time.time() < end:
                        time.sleep(0.02)
                    time.sleep(0.3)
                    if route == "fast":
                        r = fast_call("settext 71680"); time.sleep(0.1); r += fast_call("done")
                    else:
                        scr = Minecraft.getInstance().screen
                        scr.setMessage("71680"); scr.onClose(); r = "java"
                    time.sleep(1.5)
                    out.append({"route": route, "reply": r, "screen": ms.screen_name(),
                                "has_71680": any("71,680" in item_lore(it) or "71,680" in item_name(it) for it in ms.container_get_items() or [])})
                    ensure_clean(); time.sleep(0.5)
            reply({"type": "benchsign4", "out": out})
        elif cmd == "benchsign3":  # profiling helper: set a sign's text with one TextFieldHelper call
            res = {}
            with action():
                open_product("SEEDS")
                press_name("Create Buy Order")
                press_name("Custom Amount")
                end = time.time() + 3
                while "Sign" not in screen_class() and time.time() < end:
                    time.sleep(0.05)
                scr = Minecraft.getInstance().screen
                try:
                    t = time.time()
                    scr.setMessage("71680")
                    res["setMessage_s"] = round(time.time() - t, 3)
                except Exception as e:
                    res["err"] = str(e)
                try:
                    scr.onClose()
                except Exception as e:
                    res["close_err"] = str(e)
                time.sleep(0.8)
                res["screen_after"] = ms.screen_name()
                res["mentions_71680"] = any("71,680" in item_lore(it) or "71,680" in item_name(it) for it in ms.container_get_items() or [])
                ensure_clean()
            reply({"type": "benchsign3", **res})
        elif cmd == "benchsign2":  # profiling helper: what can the sign editor screen do in one call?
            res = {}
            with action():
                open_product("SEEDS")
                press_name("Create Buy Order")
                press_name("Custom Amount")
                end = time.time() + 3
                while "Sign" not in screen_class() and time.time() < end:
                    time.sleep(0.05)
                scr = Minecraft.getInstance().screen
                k = scr.getClass()
                names = []
                for c in (k, k.getSuperclass()):
                    try:
                        fs, ms_ = c.getDeclaredFields(), c.getDeclaredMethods()
                        names.append([str(c.getName()), [str(fs[i].getName()) for i in range(len(fs))], [str(ms_[i].getName()) for i in range(len(ms_))]])
                    except Exception as e:
                        names.append(["err", str(e), ""])
                res["classes"] = names
                try:
                    scr.onClose()
                except Exception as e:
                    res["close_err"] = str(e)
                time.sleep(0.5)
                ensure_clean()
            reply({"type": "benchsign2", **res})
        elif cmd == "benchsign":  # profiling helper: what does ms.screen_name() say while a sign editor is open?
            res = {}
            with action():
                open_product("SEEDS")
                press_name("Create Buy Order")
                press_name("Custom Amount")
                t = time.time(); seen = []
                while time.time() - t < 3:
                    n = ms.screen_name()
                    if not seen or seen[-1][1] != n:
                        seen.append((round(time.time() - t, 2), n))
                    time.sleep(0.02)
                    if len(seen) > 8:
                        break
                res["screen_names"] = seen
                res["class_now"] = screen_class()
                try:
                    Minecraft.getInstance().screen.onClose()
                except Exception as e:
                    res["close_err"] = str(e)
                time.sleep(0.5)
                ensure_clean()
            reply({"type": "benchsign", **res})
        elif cmd == "insert_sacks":
            with action():
                moved = insert_into_sacks(repeat=True)
            reply({"type": "insert_sacks", "moved": moved})
        elif cmd == "sellinv":
            with action():
                placed, failed, skipped = sell_inventory(msg.get("prices") or {})
            reply({"type": "sellinv", "placed": placed, "failed": failed, "skipped": skipped})
            reply({"type": "balance", "purse": purse(), "bank": _bank["value"]})
        elif cmd == "pickup":   # take everything out of the stash and Lost & Found
            res = {}
            with action():
                if msg.get("stash", True):
                    res["stash"] = pickup_stash(float(msg.get("max_secs", 180)))
                if msg.get("lost_found", True):
                    res["lost_found"] = pickup_lost_found()
            reply({"type": "pickup", **res})
        elif cmd == "bench":  # profiling helper: how long do the menu reads that every action is built from take?
            res = {}
            with action():
                t = time.time(); ms.execute("/bz"); n = 0
                while time.time() - t < 5 and slot_with("Manage Orders") is None:
                    time.sleep(0.02); n += 1
                res["open_bz_s"] = round(time.time() - t, 3)
                def avg(fn, k=12):
                    t0 = time.time()
                    for _ in range(k):
                        fn()
                    return round((time.time() - t0) / k, 4)
                res["container_get_items_s"] = avg(lambda: ms.container_get_items())
                res["menu_sig_s"] = avg(menu_sig)
                res["slot_with_s"] = avg(lambda: slot_with("Manage Orders"))
                res["screen_class_s"] = avg(screen_class)
                try:
                    res["screen_name_fn_s"] = avg(ms.screen_name)
                    res["screen_name"] = str(ms.screen_name())
                except Exception as e:
                    res["screen_name_err"] = str(e)
                res["screen_class"] = screen_class()
                def _m():
                    return Minecraft.getInstance()
                res["getInstance_s"] = avg(_m)
                mc = Minecraft.getInstance()
                res["mc.player_s"] = avg(lambda: mc.player)
                pl = mc.player
                res["mc.gameMode_s"] = avg(lambda: mc.gameMode)
                res["player.containerMenu_s"] = avg(lambda: pl.containerMenu)
                cm = pl.containerMenu
                res["menu.containerId_s"] = avg(lambda: cm.containerId)
                res["same_obj_cached_attr_s"] = avg(lambda: cm.containerId)
                res["mc.screen_cached_s"] = avg(lambda: mc.screen)
                res["mc_screen_attr_s"] = avg(lambda: Minecraft.getInstance().screen)
                res["player_inventory_s"] = avg(lambda: ms.player_inventory())
                res["inv_count_s"] = avg(lambda: inv_count("SEEDS"))
                t0 = time.time(); ensure_clean(); res["ensure_clean_s"] = round(time.time() - t0, 3)
            reply({"type": "bench", **res})
        elif cmd == "invcount":  # fast: reads the inventory only, never opens a menu
            reply(inv_report(msg["product"]))
        elif cmd == "sackscan":  # slow (opens every sack once); the app asks only when you press Scan sacks
            with action():
                res = sack_scan()
            reply(res)
        elif cmd == "invall":  # fast: whole inventory, per product, never opens a menu
            reply(inv_all())
        elif cmd == "inv":
            reply({"type": "log", "msg": json.dumps([(it.item, it.count, strip_fmt(str(it.nbt))[:120]) for it in inventory_items()])})
        elif cmd == "screen":
            reply({"type": "log", "msg": screen_class() + " | " + str(ms.screen_name())})
        elif cmd == "click":
            with _lock:
                if "slot" in msg:
                    click_slot(int(msg["slot"]), int(msg.get("button", 0)))
                else:
                    press_name(msg["name"], exact=bool(msg.get("exact")), button=int(msg.get("button", 0)))
                time.sleep(0.7)
            reply({"type": "dump", "screen": screen_class(), "items": dump_items(lore=bool(msg.get("lore")))})
        elif cmd == "sign":
            with _lock:
                type_in_sign(str(msg["text"]))
            reply({"type": "dump", "screen": screen_class(), "items": dump_items()})
        elif cmd == "close":
            with _lock:
                ensure_clean()
            reply({"type": "log", "msg": "closed"})
        elif cmd in ("open", "probe"):
            with _lock:
                open_product(msg["product"])
            reply({"type": "dump", "screen": screen_class(), "items": dump_items(lore=bool(msg.get("lore")))})
        elif cmd == "bookwatch":   # {"products": [ids]}: keep reading these products' order books; [] stops
            _book["products"] = [str(x) for x in (msg.get("products") or [])][:12]
            _book["focus"] = msg.get("focus") or None
            if (_book["products"] or _book["focus"]) and _book["thread"] is None:
                _book["gen"] += 1
                _book["thread"] = threading.Thread(target=_book_loop, args=(_book["gen"],), daemon=True)
                _book["thread"].start()
            elif not _book["products"] and not _book["focus"]:
                _book["gen"] += 1; _book["thread"] = None
            reply({"type": "bookwatch", "products": _book["products"]})
        elif cmd == "dump":
            reply({"type": "dump", "screen": screen_class(), "items": dump_items(lore=True)})
        else:
            reply({"type": "error", "msg": f"unknown cmd {cmd!r}"})
    except Exception as e:  # always report back, never go silent
        reply({"type": "error", "msg": str(e), "cmd": locals().get("cmd")})


def late_log(send_fn, since, what):
    """Confirm a sack pull from the batched [Sacks] chat line without blocking whatever runs next."""
    try:
        d = sack_chat_delta(since)
        send_fn({"type": "log", "msg": f"[Sacks] {d:+,} items {'confirmed ' + what if d < 0 else 'not seen for ' + what}"})
    except Exception:
        pass


def late_sack_signal(conn, pid, side, since, expect_positive):
    """Verify sack movement from the batched [Sacks] chat line *after* the trade has already been reported."""
    try:
        d = sack_chat_delta(since)
        ok = (d > 0) if expect_positive else (d < 0)
        send(conn, {"type": "signals", "product": pid, "side": side, "signals": {"sacks": ok},
                    "msg": f"[Sacks] {d:+,} items {'confirmed sack movement' if ok else 'not seen'}"})
    except Exception:
        pass


def run_pipeline(steps, reply):
    """Run steps in order; stop at the first failure so nothing downstream acts on a half-finished state."""
    start = purse()
    prev_op, t_buy = None, time.time()
    for i, st in enumerate(steps):
        op = st["op"]
        try:
            if op == "wait" and prev_op == "instabuy":
                # not a blind sleep: wait for the compactor/sacks to finish, which Hypixel announces as "[Sacks] +N items"
                d = sack_chat_delta(t_buy, timeout=max(float(st.get("seconds", 3)) * 3, 10.0), quiet=1.2)
                text = f"compactor done ([Sacks] {d:+,} items)" if d > 0 else "no [Sacks] line (goods stayed in the inventory)"
            elif op == "wait":
                time.sleep(float(st.get("seconds", 1)))
                text = f"waited {st.get('seconds', 1)}s"
            elif op == "instabuy":
                t_buy = time.time()
                with action():
                    text = instabuy(st["product"], st["qty"])
            elif op == "instasell":
                with action():
                    text = instasell(st["product"], st.get("qty", 1))
            elif op == "sackget":
                t0 = time.time()
                with action():
                    text = f"moved {sack_get(st['product'], st['qty'])}x {display(st['product'])} from sacks"
                threading.Thread(target=late_log, args=(reply, t0, "the pull"), daemon=True).start()
            elif op == "tradesell":
                with action():
                    sold, gained, _ = trades_sell(st["product"], st.get("max_stacks"))
                text = f"sold {sold}x {display(st['product'])} to the shop for {gained:,.1f} coins" if gained is not None else f"sold {sold}"
            else:
                raise RuntimeError(f"unknown step {op!r}")
            reply({"type": "step", "i": i, "op": op, "ok": True, "msg": text})
            prev_op = op
        except Exception as e:
            reply({"type": "step", "i": i, "op": op, "ok": False, "msg": str(e)})
            reply({"type": "pipeline_done", "ok": False, "purse_start": start, "purse_end": purse()})
            return
    reply({"type": "pipeline_done", "ok": True, "purse_start": start, "purse_end": purse()})


def serve_client(conn, addr):
    ms.echo(f"[Blockberg] terminal connected from {addr[0]}")
    _clients.append(conn)
    buf = b""
    try:
        while True:
            data = conn.recv(4096)
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                if line.strip():
                    handle(conn, line.decode())
    finally:
        if conn in _clients:
            _clients.remove(conn)
        _chat_subs.discard(conn)
        conn.close()
    ms.echo("[Blockberg] terminal disconnected")


_srv = [None]
_reloading = threading.Event()
_reload_failed = threading.Event()


def listen_socket():
    """Bind the agent port, retrying for a few seconds (a restarting agent may still be letting go of it)."""
    for _ in range(40):
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            srv.bind(("0.0.0.0", PORT))
            srv.listen(4)
            return srv
        except OSError:
            srv.close()
            time.sleep(0.25)
    raise RuntimeError(f"port {PORT} stayed busy")


# ---------- keeping the character in SkyBlock ----------
# Hypixel sometimes drops the connection and "routes you to limbo". The bazaar does not exist there, so every order read fails.
# A watchdog notices that (scoreboard without a purse, or the limbo chat line) and goes back: /lobby, then /play skyblock.
_LIMBO_LINES = ("spawned in limbo", "routed to limbo", "sending you to limbo", "you are in limbo")
_rejoin_hint = [0.0]          # when an action noticed the bazaar was unavailable (makes the watchdog act sooner)
_rejoin_state = {"tries": 0, "last": 0.0, "bad_since": None, "limbo_seen": 0.0}


def where() -> str:
    """"skyblock" when the sidebar shows the SkyBlock purse, "limbo" after a limbo chat line, otherwise "other" / "unknown"."""
    try:
        sb = ms.get_scoreboard()
    except Exception:
        return "unknown"
    if sb:
        title = strip_fmt(str(getattr(sb, "title", "") or "")).upper()
        for e in sb.entries:
            text = strip_fmt(e.display_name) + " " + strip_fmt(e.name)
            if re.search(r"(?:Purse|Piggy):", text):
                return "skyblock"
        if "SKYBLOCK" in title and sb.entries:
            return "skyblock"
    if any(any(k in t.lower() for k in _LIMBO_LINES) and ts > _rejoin_state["limbo_seen"] for ts, t in list(_syslog)):
        return "limbo"
    return "other"


def rejoin_skyblock(timeout=60) -> str:
    """From limbo or a lobby back into SkyBlock. Returns a short result line; raises if it did not work."""
    st = where()
    if st == "skyblock":
        return "already in SkyBlock"
    push({"type": "log", "msg": f"[auto-rejoin] not in SkyBlock ({st}): going back (/lobby, /play skyblock)"})
    ms.execute("/lobby")
    time.sleep(4.0)
    ms.execute("/play skyblock")
    end = time.time() + timeout
    while time.time() < end:
        time.sleep(1.5)
        if where() == "skyblock":
            time.sleep(2.0)
            _rejoin_state["limbo_seen"] = time.time()
            return "back in SkyBlock"
    raise RuntimeError("did not reach SkyBlock within %d s" % timeout)


def rejoin_loop():
    """Background watchdog: if the character is not in SkyBlock for a while, bring it back (tries again with a growing pause)."""
    st = _rejoin_state
    while True:
        time.sleep(8)
        if _reloading.is_set():
            continue
        try:
            w = where()
            if w == "skyblock":
                st["bad_since"] = None; st["tries"] = 0
                continue
            if w == "unknown":
                continue   # no world loaded at all (title screen): nothing the game can do from here
            now = time.time()
            if st["bad_since"] is None:
                st["bad_since"] = now
            # a limbo line or a failed bazaar read means we really are out; an empty sidebar alone could just be a server hop
            sure = w == "limbo" or now - _rejoin_hint[0] < 120
            if now - st["bad_since"] < (10 if sure else 75):
                continue
            if now - st["last"] < min(30 * (st["tries"] + 1), 180):
                continue
            st["last"] = now; st["tries"] += 1
            with action():
                res = rejoin_skyblock()
            push({"type": "log", "msg": f"[auto-rejoin] {res}"})
            st["bad_since"] = None
        except Exception as e:
            push({"type": "log", "msg": f"[auto-rejoin] failed: {e}"})


def reload_agent():
    """Restart this agent from the app (Settings → Restart agent) so a newly installed version loads:
    free the port, start a fresh copy of the script, and exit once it answers. If it never comes up,
    take the port back and keep running, so the app is never left without an agent."""
    _reloading.set()
    time.sleep(0.3)  # let the reply reach the app
    try:
        _srv[0].close()
    except OSError:
        pass
    ms.execute("\\blockberg_agent")
    end = time.time() + 10
    while time.time() < end:
        time.sleep(0.4)
        try:
            socket.create_connection(("127.0.0.1", PORT), timeout=0.5).close()
            for c in list(_clients):
                try:
                    c.close()
                except OSError:
                    pass
            os._exit(0)  # the new agent has the port
        except OSError:
            pass
    ms.echo("[Blockberg] restart failed: the new agent didn't start; keeping this one")
    _reload_failed.set()


def main():
    threading.Thread(target=chat_loop, daemon=True).start()
    threading.Thread(target=ensure_fast, daemon=True).start()   # start / find the in-game fast helper
    threading.Thread(target=rejoin_loop, daemon=True).start()   # back to SkyBlock after a limbo / lobby drop
    while True:
        srv = listen_socket()
        _srv[0] = srv
        ms.echo(f"[Blockberg] agent listening on :{PORT}")
        try:
            while True:
                conn, addr = srv.accept()
                threading.Thread(target=serve_client, args=(conn, addr), daemon=True).start()
        except OSError:
            if not _reloading.is_set():
                raise
            _reload_failed.wait()  # only returns if the restart failed: serve again
            _reloading.clear()
            _reload_failed.clear()


main()
