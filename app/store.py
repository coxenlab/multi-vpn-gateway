"""SQLite + 凭据字段级加密(Fernet)。主密钥存数据卷里权限锁死的文件(容器内读不到 macOS 钥匙串)。"""
import os, sqlite3
import json
from cryptography.fernet import Fernet

DATA_DIR = os.environ.get("DATA_DIR", "/data")
DB = os.path.join(DATA_DIR, "vpnmgr.db")
KEYF = os.path.join(DATA_DIR, "master.key")


def _key():
    os.makedirs(DATA_DIR, exist_ok=True)
    if not os.path.exists(KEYF):
        fd = os.open(KEYF, os.O_WRONLY | os.O_CREAT, 0o600)
        os.write(fd, Fernet.generate_key())
        os.close(fd)
    with open(KEYF, "rb") as f:
        return f.read()


F = Fernet(_key())


def _c():
    os.makedirs(DATA_DIR, exist_ok=True)
    c = sqlite3.connect(DB)
    c.row_factory = sqlite3.Row
    return c


def init():
    with _c() as c:
        c.executescript(
            """
            CREATE TABLE IF NOT EXISTS channels(
              id TEXT PRIMARY KEY, name TEXT, vpn_type TEXT, server TEXT, ec_ver TEXT,
              login_method TEXT, username TEXT, password_enc TEXT, vnc_password TEXT,
              mac TEXT, novnc_port INTEGER, probe_url TEXT, status TEXT, container_id TEXT);
            CREATE TABLE IF NOT EXISTS domains(
              id INTEGER PRIMARY KEY AUTOINCREMENT, channel_id TEXT, pattern TEXT);
            CREATE TABLE IF NOT EXISTS rules(
              id INTEGER PRIMARY KEY AUTOINCREMENT, channel_id TEXT,
              kind TEXT, pattern TEXT, enabled INTEGER DEFAULT 1);
            CREATE TABLE IF NOT EXISTS mirrors(
              id INTEGER PRIMARY KEY AUTOINCREMENT, host TEXT UNIQUE,
              priority INTEGER, enabled INTEGER DEFAULT 1);
            """
        )
        cols = [r[1] for r in c.execute("PRAGMA table_info(channels)").fetchall()]
        if "latency_ms" not in cols:
            c.execute("ALTER TABLE channels ADD COLUMN latency_ms INTEGER")
        if "config_json" not in cols:
            c.execute("ALTER TABLE channels ADD COLUMN config_json TEXT")
        # 旧 domains 一次性迁入 rules(仅当 rules 为空)
        if c.execute("SELECT COUNT(*) FROM rules").fetchone()[0] == 0:
            for r in c.execute("SELECT channel_id, pattern FROM domains").fetchall():
                c.execute(
                    "INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES(?,?,?,1)",
                    (r["channel_id"], "domain", r["pattern"]),
                )
        if c.execute("SELECT COUNT(*) FROM mirrors").fetchone()[0] == 0:
            for i, h in enumerate(("docker.1ms.run", "hub.rat.dev"), start=1):
                c.execute("INSERT INTO mirrors(host,priority,enabled) VALUES(?,?,1)", (h, i))


def _row(r):
    d = dict(r)
    d.pop("password_enc", None)  # 永不把密码回传给前端
    raw = d.pop("config_json", None)
    d["config"] = _public_config(raw)  # 剥除 secret 字段后才回前端
    return d


def _clean_field(key, val):
    """文本字段去空白。账号/网关去掉所有空白(不含合法空格,误输空格是认证/连接失败常见坑
    ——曾因账号尾随空格被网关拒登);其余文本去首尾空白。密码不在此处理(可能合法含空格)。"""
    if val is None:
        return val
    s = str(val)
    return "".join(s.split()) if key in ("username", "server") else s.strip()


# config_json 内部格式:{"_fields": {...明文/密文混合...}, "_secret": [..secret key..]}
# secret 字段值是 Fernet 密文字符串;非 secret 是明文。
def _enc_config(config, secret_keys):
    if not config:
        return None
    sk = set(secret_keys)
    fields = {}
    for k, v in config.items():
        fields[k] = F.encrypt(str(v).encode()).decode() if k in sk else _clean_field(k, v)
    return json.dumps({"_fields": fields, "_secret": sorted(sk)})


def _public_config(raw):
    """供前端:解析 config_json,剥除所有 secret 字段(命门 #5)。"""
    if not raw:
        return {}
    obj = json.loads(raw)
    sk = set(obj.get("_secret", []))
    return {k: v for k, v in obj.get("_fields", {}).items() if k not in sk}


def add_channel(ch, config=None, secret_keys=None):
    pw = F.encrypt(ch["password"].encode()).decode() if ch.get("password") else ""
    cfg_json = _enc_config(config or {}, secret_keys or [])
    with _c() as c:
        c.execute(
            """INSERT INTO channels(id,name,vpn_type,server,ec_ver,login_method,
               username,password_enc,vnc_password,mac,probe_url,status,config_json)
               VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)""",
            (ch["id"], _clean_field("name", ch["name"]), ch["vpn_type"],
             _clean_field("server", ch["server"]), ch["ec_ver"], ch["login_method"],
             _clean_field("username", ch["username"]), pw, ch["vnc_password"], ch["mac"],
             _clean_field("probe_url", ch["probe_url"]), ch["status"], cfg_json),
        )


def update_channel(cid, fields, secret_keys=()):
    """编辑已有通道:更新允许的列 + 把 server/username/password 镜像进 config_json
    (oss 从 config 读)。文本字段经 _clean_field 去空白。只更新 fields 里出现的键。"""
    sk = set(secret_keys)
    sets, vals = [], []
    for col in ("name", "server", "ec_ver", "username", "probe_url"):
        if col in fields:
            sets.append(f"{col}=?")
            vals.append(_clean_field(col, fields[col]))
    if "password" in fields:
        pw = fields["password"]
        sets.append("password_enc=?")
        vals.append(F.encrypt(pw.encode()).decode() if pw else "")
    if sets:
        with _c() as c:
            c.execute(f"UPDATE channels SET {', '.join(sets)} WHERE id=?", vals + [cid])
    # 镜像连接字段到 config_json(oss_connect 从 config 读;密码 secret 加密,不清洗)
    for k in ("server", "username", "password"):
        if k in fields:
            secret = k in sk
            set_config_field(cid, k, fields[k] if secret else _clean_field(k, fields[k]),
                             secret=secret)


def set_container(cid, container_id, novnc, status):
    with _c() as c:
        c.execute("UPDATE channels SET container_id=?,novnc_port=?,status=? WHERE id=?",
                  (container_id, novnc, status, cid))


def set_status(cid, status):
    with _c() as c:
        c.execute("UPDATE channels SET status=? WHERE id=?", (status, cid))


def set_novnc_port(cid, port):
    with _c() as c:
        c.execute("UPDATE channels SET novnc_port=? WHERE id=?", (port, cid))


def get_channel(cid):
    with _c() as c:
        r = c.execute("SELECT * FROM channels WHERE id=?", (cid,)).fetchone()
        return _row(r) if r else None


def get_password(cid):
    with _c() as c:
        r = c.execute("SELECT password_enc FROM channels WHERE id=?", (cid,)).fetchone()
        return F.decrypt(r["password_enc"].encode()).decode() if r and r["password_enc"] else ""


def get_config_raw(cid):
    """原始 config_json 文本(测试/调试用;secret 字段为密文)。"""
    with _c() as c:
        r = c.execute("SELECT config_json FROM channels WHERE id=?", (cid,)).fetchone()
        return r["config_json"] if r and r["config_json"] else ""


def get_config(cid):
    """解密后的完整 config(含 secret 明文)。仅供容器注入,绝不回前端。"""
    raw = get_config_raw(cid)
    if not raw:
        return {}
    obj = json.loads(raw)
    sk = set(obj.get("_secret", []))
    out = {}
    for k, v in obj.get("_fields", {}).items():
        out[k] = F.decrypt(v.encode()).decode() if k in sk else v
    return out


def set_config_field(cid, key, value, secret=False):
    """把单个字段合并进现有 config_json(read-modify-write)。

    secret=False(默认):明文存(供前端展示,如上传安装包文件名引用)。
    secret=True:Fernet 加密后存,并登记到 _secret(回前端时由 _public_config 剥除)。
    """
    raw = get_config_raw(cid)
    obj = json.loads(raw) if raw else {"_fields": {}, "_secret": []}
    obj.setdefault("_fields", {})
    obj.setdefault("_secret", [])
    obj["_fields"][key] = F.encrypt(str(value).encode()).decode() if secret else value
    if secret and key not in obj["_secret"]:
        obj["_secret"] = sorted(set(obj["_secret"]) | {key})
    with _c() as c:
        c.execute("UPDATE channels SET config_json=? WHERE id=?", (json.dumps(obj), cid))


def list_channels():
    with _c() as c:
        return [_row(r) for r in c.execute("SELECT * FROM channels").fetchall()]


def del_channel(cid):
    with _c() as c:
        c.execute("DELETE FROM channels WHERE id=?", (cid,))
        c.execute("DELETE FROM domains WHERE channel_id=?", (cid,))
        c.execute("DELETE FROM rules WHERE channel_id=?", (cid,))


def add_rule(cid, kind, pattern):
    with _c() as c:
        cur = c.execute(
            "INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES(?,?,?,1)",
            (cid, kind, pattern),
        )
        return cur.lastrowid


def list_rules(cid):
    with _c() as c:
        return [dict(r) for r in c.execute(
            "SELECT id,channel_id,kind,pattern,enabled FROM rules WHERE channel_id=?",
            (cid,)).fetchall()]


def all_rules():
    with _c() as c:
        return [dict(r) for r in c.execute(
            "SELECT id,channel_id,kind,pattern,enabled FROM rules").fetchall()]


def get_rule(rid):
    with _c() as c:
        r = c.execute("SELECT * FROM rules WHERE id=?", (rid,)).fetchone()
        return dict(r) if r else None


def del_rule(rid):
    with _c() as c:
        c.execute("DELETE FROM rules WHERE id=?", (rid,))


def set_rule_enabled(rid, enabled):
    with _c() as c:
        c.execute("UPDATE rules SET enabled=? WHERE id=?", (1 if enabled else 0, rid))


def set_latency(cid, ms):
    with _c() as c:
        c.execute("UPDATE channels SET latency_ms=? WHERE id=?", (ms, cid))


def add_mirror(host):
    with _c() as c:
        nextp = c.execute("SELECT COALESCE(MAX(priority),0)+1 FROM mirrors").fetchone()[0]
        cur = c.execute("INSERT INTO mirrors(host,priority,enabled) VALUES(?,?,1)", (host, nextp))
        return cur.lastrowid


def list_mirrors():
    with _c() as c:
        return [dict(r) for r in c.execute(
            "SELECT id,host,priority,enabled FROM mirrors ORDER BY priority").fetchall()]


def del_mirror(mid):
    with _c() as c:
        c.execute("DELETE FROM mirrors WHERE id=?", (mid,))


def set_mirror(mid, priority=None, enabled=None):
    sets, vals = [], []
    if priority is not None:
        sets.append("priority=?"); vals.append(int(priority))
    if enabled is not None:
        sets.append("enabled=?"); vals.append(1 if enabled else 0)
    if sets:
        with _c() as c:
            c.execute(f"UPDATE mirrors SET {', '.join(sets)} WHERE id=?", vals + [mid])
