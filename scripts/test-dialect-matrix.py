#!/usr/bin/env python3
"""Ephemeral multi-dialect matrix verification against cached Postgres, MySQL, and MariaDB images."""
from __future__ import annotations

import base64
import ctypes
import glob
import hashlib
import hmac
import os
from pathlib import Path
import re
import select
import signal
import socket
import struct
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]
PG_PORT = 36432
MY_PORT = 36306
MDB_PORT = 36307

ALL_OPERATIONS = [
    "CREATE_SCHEMA",
    "DROP_SCHEMA",
    "CREATE_TABLE",
    "CREATE_TABLE_AS",
    "ALTER_TABLE",
    "ALTER_TABLE_ADD_COLUMN",
    "ALTER_TABLE_DROP_COLUMN",
    "ALTER_TABLE_RENAME_TABLE",
    "ALTER_TABLE_RENAME_COLUMN",
    "ALTER_TABLE_COLUMN_TYPE",
    "ALTER_TABLE_COLUMN_DEFAULT",
    "ALTER_TABLE_COLUMN_NULLABILITY",
    "ALTER_TABLE_ADD_CONSTRAINT",
    "ALTER_TABLE_DROP_CONSTRAINT",
    "CREATE_INDEX_BTREE",
    "CREATE_INDEX_UNIQUE_BTREE",
    "DROP_TABLE",
    "CREATE_INDEX",
    "DROP_INDEX",
]


class PgError(Exception):
    def __init__(self, sqlstate: str, message: str):
        super().__init__(f"{sqlstate}: {message}")
        self.sqlstate = sqlstate
        self.message = message


class PgClient:
    """Minimal PG wire v3 + SCRAM-SHA-256 client over stdlib TCP sockets."""

    def __init__(self, host: str, port: int, user: str, password: str, database: str):
        self.sock = socket.create_connection((host, port), timeout=15)
        self._negotiate(user, password, database)

    def close(self):
        try:
            self._send_pkt(b"X", b"")
        except OSError:
            pass
        self.sock.close()

    def _recv_exact(self, n: int) -> bytes:
        buf = bytearray()
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise EOFError("unexpected EOF from fedengine")
            buf.extend(chunk)
        return bytes(buf)

    def _recv_msg(self) -> tuple[bytes, bytes]:
        tag = self._recv_exact(1)
        length = struct.unpack(">I", self._recv_exact(4))[0]
        payload = self._recv_exact(length - 4)
        return tag, payload

    def _send_pkt(self, tag: bytes, body: bytes):
        self.sock.sendall(tag + struct.pack(">I", len(body) + 4) + body)

    def _negotiate(self, user: str, password: str, database: str):
        self.sock.sendall(bytes([0, 0, 0, 8, 4, 210, 22, 47]))
        assert self._recv_exact(1) == b"N"
        startup = (
            b"\x00\x03\x00\x00user\x00"
            + user.encode()
            + b"\x00database\x00"
            + database.encode()
            + b"\x00\x00"
        )
        self.sock.sendall(struct.pack(">I", len(startup) + 4) + startup)

        nonce = base64.b64encode(os.urandom(18)).decode("ascii")
        client_first_bare = f"n=,r={nonce}"
        salted = b""
        auth_message = b""

        while True:
            tag, payload = self._recv_msg()
            if tag == b"R":
                code = struct.unpack(">I", payload[:4])[0]
                if code == 10:
                    init = ("n,," + client_first_bare).encode()
                    body = b"SCRAM-SHA-256\x00" + struct.pack(">I", len(init)) + init
                    self._send_pkt(b"p", body)
                elif code == 11:
                    server_first = payload[4:].decode()
                    kv = dict(item.split("=", 1) for item in server_first.split(","))
                    salt = base64.b64decode(kv["s"])
                    iters = int(kv["i"])
                    r_val = kv["r"]
                    assert r_val.startswith(nonce)
                    salted = hashlib.pbkdf2_hmac("sha256", password.encode("utf-8"), salt, iters, 32)
                    client_key = hmac.new(salted, b"Client Key", hashlib.sha256).digest()
                    stored_key = hashlib.sha256(client_key).digest()
                    client_final_no_proof = f"c=biws,r={r_val}"
                    auth_message = f"{client_first_bare},{server_first},{client_final_no_proof}".encode()
                    sig = hmac.new(stored_key, auth_message, hashlib.sha256).digest()
                    proof = bytes(a ^ b for a, b in zip(client_key, sig))
                    body = f"{client_final_no_proof},p={base64.b64encode(proof).decode('ascii')}".encode()
                    self._send_pkt(b"p", body)
                elif code == 12:
                    server_final = payload[4:].decode()
                    server_key = hmac.new(salted, b"Server Key", hashlib.sha256).digest()
                    expected = base64.b64encode(
                        hmac.new(server_key, auth_message, hashlib.sha256).digest()
                    ).decode("ascii")
                    assert server_final == f"v={expected}"
                elif code == 0:
                    pass
                else:
                    raise RuntimeError(f"unsupported auth code {code}")
            elif tag == b"E":
                raise self._parse_err(payload)
            elif tag == b"Z":
                return

    @staticmethod
    def _parse_err(payload: bytes) -> PgError:
        fields = {}
        i = 0
        while i < len(payload):
            ftype = payload[i : i + 1]
            i += 1
            if ftype == b"\x00":
                break
            end = payload.index(b"\x00", i)
            fields[ftype.decode()] = payload[i:end].decode(errors="replace")
            i = end + 1
        return PgError(fields.get("C", ""), fields.get("M", ""))

    def _read_until_ready(self) -> tuple[list[list[str | None]], str]:
        rows: list[list[str | None]] = []
        cmd_tag = ""
        err: PgError | None = None
        while True:
            tag, payload = self._recv_msg()
            if tag == b"D":
                ncols = struct.unpack(">H", payload[:2])[0]
                pos = 2
                row: list[str | None] = []
                for _ in range(ncols):
                    size = struct.unpack(">i", payload[pos : pos + 4])[0]
                    pos += 4
                    if size == -1:
                        row.append(None)
                    else:
                        row.append(payload[pos : pos + size].decode())
                        pos += size
                rows.append(row)
            elif tag == b"C":
                cmd_tag = payload.rstrip(b"\x00").decode()
            elif tag == b"E":
                err = self._parse_err(payload)
            elif tag == b"Z":
                if err is not None:
                    raise err
                return rows, cmd_tag

    def simple(self, sql: str) -> tuple[list[list[str | None]], str]:
        self._send_pkt(b"Q", sql.encode() + b"\x00")
        return self._read_until_ready()

    def exec_typed(
        self, sql: str, param_oids: list[int], values: list[str | None]
    ) -> tuple[list[list[str | None]], str]:
        parse = b"\x00" + sql.encode() + b"\x00" + struct.pack(">H", len(param_oids))
        for oid in param_oids:
            parse += struct.pack(">i", oid)
        bind = b"\x00\x00\x00\x00" + struct.pack(">H", len(values))
        for val in values:
            if val is None:
                bind += struct.pack(">i", -1)
            else:
                b = val.encode()
                bind += struct.pack(">i", len(b)) + b
        bind += struct.pack(">H", 0)
        self._send_pkt(b"P", parse)
        self._send_pkt(b"B", bind)
        self._send_pkt(b"D", b"P\x00")
        self._send_pkt(b"E", b"\x00\x00\x00\x00\x00")
        self._send_pkt(b"S", b"")
        return self._read_until_ready()


def find_duckdb_lib() -> Path:
    matches = sorted((ROOT / "target" / "duckdb-download").glob("*/v2.0.0/libduckdb.so"))
    if not matches:
        matches = sorted(Path("/tmp").glob("libduckdb.so"))
    if not matches:
        raise RuntimeError("target/duckdb-download/*/v2.0.0/libduckdb.so missing; build fedengine first")
    return matches[0]


def discover_extension_versions(ext_dir: Path, libduckdb: Path) -> dict[str, str]:
    lib = ctypes.CDLL(str(libduckdb))
    lib.duckdb_value_varchar.restype = ctypes.c_void_p
    cfg, db, conn = ctypes.c_void_p(), ctypes.c_void_p(), ctypes.c_void_p()
    err = ctypes.c_char_p()
    assert lib.duckdb_create_config(ctypes.byref(cfg)) == 0
    assert lib.duckdb_set_config(cfg, b"allow_unsigned_extensions", b"true") == 0
    assert lib.duckdb_open_ext(b":memory:", ctypes.byref(db), cfg, ctypes.byref(err)) == 0
    assert lib.duckdb_connect(db, ctypes.byref(conn)) == 0
    res = ctypes.create_string_buffer(256)
    sql = (
        f"SET extension_directory='{ext_dir}'; "
        "LOAD postgres; LOAD mysql; "
        "SELECT extension_name, extension_version FROM duckdb_extensions() "
        "WHERE extension_name IN ('postgres_scanner','mysql_scanner') ORDER BY 1"
    )
    assert lib.duckdb_query(conn, sql.encode(), ctypes.byref(res)) == 0
    out = {}
    for row_idx in range(2):
        p_name = lib.duckdb_value_varchar(ctypes.byref(res), 0, row_idx)
        p_ver = lib.duckdb_value_varchar(ctypes.byref(res), 1, row_idx)
        out[ctypes.string_at(p_name).decode()] = ctypes.string_at(p_ver).decode()
        lib.duckdb_free(ctypes.c_void_p(p_name))
        lib.duckdb_free(ctypes.c_void_p(p_ver))
    lib.duckdb_destroy_result(ctypes.byref(res))
    lib.duckdb_disconnect(ctypes.byref(conn))
    lib.duckdb_close(ctypes.byref(db))
    lib.duckdb_destroy_config(ctypes.byref(cfg))
    return out


def ensure_cached_image(image: str):
    subprocess.run(["docker", "image", "inspect", image], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)


def wait_container_ready(name: str, check_cmd: list[str], timeout_s: int = 90):
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        res = subprocess.run(["docker", "exec", name] + check_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if res.returncode == 0:
            return
        time.sleep(1)
    raise RuntimeError(f"container {name} failed to become ready")


def assert_err_sqlstate(fn, expected_state: str):
    try:
        fn()
    except PgError as exc:
        assert exc.sqlstate == expected_state, f"expected SQLSTATE {expected_state}, got {exc}"
        return
    raise AssertionError(f"expected PgError {expected_state}, succeeded unexpectedly")


def run_dialect_checks(pg: PgClient, alias: str, schema: str, alt_schema: str, dialect: str):
    # v6 CREATE TABLE + conditional IF NOT EXISTS
    extra_check = ", CHECK(qty > 0)" if dialect in ("postgres", "mariadb") else ""
    if dialect == "mysql":
        assert_err_sqlstate(
            lambda: pg.simple(f"CREATE TABLE {alias}.{schema}.m_chk(qty INTEGER, CHECK(qty > 0))"),
            "0A000",
        )
    pg.simple(f"CREATE TABLE {alias}.{schema}.m_parent(id INTEGER PRIMARY KEY)")
    create_child = (
        f"CREATE TABLE IF NOT EXISTS {alias}.{schema}.m_child("
        "id INTEGER PRIMARY KEY, "
        "code VARCHAR(32) NOT NULL UNIQUE DEFAULT 'dflt', "
        "parent_id INTEGER, "
        "qty INTEGER NOT NULL DEFAULT 1, "
        "flag BOOLEAN, "
        "at_t TIME, "
        "raw_b BYTEA, "
        "uid UUID, "
        f"FOREIGN KEY(parent_id) REFERENCES {alias}.{schema}.m_parent(id)"
        f"{extra_check})"
    )
    pg.simple(create_child)
    pg.simple(create_child)  # idempotent conditional repeat

    for _ in range(2):
        pg.simple(f"ALTER TABLE {alias}.{schema}.m_child ADD COLUMN IF NOT EXISTS note VARCHAR(64) NOT NULL DEFAULT 'n1'")
    for _ in range(2):
        pg.simple(f"ALTER TABLE {alias}.{schema}.m_child DROP COLUMN IF EXISTS note")

    pg.simple(f"INSERT INTO {alias}.{schema}.m_parent VALUES (10)")
    if dialect in ("postgres", "mariadb"):
        # Enforced CHECK + FK check
        failed_chk = False
        try:
            pg.simple(f"INSERT INTO {alias}.{schema}.m_child(id, code, parent_id, qty) VALUES(1, 'bad_c', 10, 0)")
        except PgError:
            failed_chk = True
        assert failed_chk, f"{dialect}: CHECK constraint was not enforced"
    failed_fk = False
    try:
        pg.simple(f"INSERT INTO {alias}.{schema}.m_child(id, code, parent_id, qty) VALUES(2, 'bad_fk', 999, 1)")
    except PgError:
        failed_fk = True
    assert failed_fk, f"{dialect}: FOREIGN KEY constraint was not enforced"

    # Typed param reuse ($2, $1, $2 ...) + boolean/time/bytea/uuid
    # OIDs: TEXT=25, INT4=23, BOOL=16, TIME=1083, BYTEA=17, UUID=2950
    oids = [25, 23, 23, 16, 1083, 17, 2950]
    vals = ["reused", "10", "5", "t", "12:34:56", "\\x0102abff", "550e8400-e29b-41d4-a716-446655440000"]
    pg.exec_typed(
        f"INSERT INTO {alias}.{schema}.m_child(id, code, parent_id, qty, flag, at_t, raw_b, uid) "
        "VALUES ($2, $1, $2, $3, $4, $5, $6, $7)",
        oids,
        vals,
    )
    rows, _ = pg.simple(
        f"SELECT code, parent_id::INTEGER, qty::INTEGER, flag, at_t::VARCHAR, raw_b::BYTEA, uid::VARCHAR "
        f"FROM {alias}.{schema}.m_child WHERE id=10"
    )
    assert len(rows) == 1, f"{dialect}: unexpected row count {rows}"
    code, pid, qty, flag, at_t, raw_b, uid = rows[0]
    assert code == "reused" and pid == "10" and qty == "5", rows[0]
    assert flag in ("t", "true"), rows[0]
    assert at_t is not None and at_t.startswith("12:34:56"), rows[0]
    assert raw_b is not None and raw_b.lower() == "\\x0102abff", rows[0]
    assert uid is not None and uid.lower() == "550e8400-e29b-41d4-a716-446655440000", rows[0]

    # Out-of-order + reuse UPDATE
    pg.exec_typed(
        f"UPDATE {alias}.{schema}.m_child SET code=$2 WHERE id=$1 AND parent_id=$1",
        [23, 25],
        ["10", "upd"],
    )
    rows, _ = pg.simple(f"SELECT code FROM {alias}.{schema}.m_child WHERE id=10")
    assert rows == [["upd"]], rows

    # DROP NOT NULL / SET NOT NULL preserving DEFAULT 'dflt'
    pg.simple(f"ALTER TABLE {alias}.{schema}.m_child ALTER COLUMN code DROP NOT NULL")
    pg.simple(f"INSERT INTO {alias}.{schema}.m_child(id, code, parent_id, qty) VALUES(11, NULL, 10, 1)")
    pg.simple(f"UPDATE {alias}.{schema}.m_child SET code='k11' WHERE id=11")
    pg.simple(f"ALTER TABLE {alias}.{schema}.m_child ALTER COLUMN code SET NOT NULL")
    null_rejected = False
    try:
        pg.simple(f"INSERT INTO {alias}.{schema}.m_child(id, code, parent_id, qty) VALUES(12, NULL, 10, 1)")
    except PgError:
        null_rejected = True
    assert null_rejected, f"{dialect}: SET NOT NULL did not reject NULL insert"
    pg.simple(f"INSERT INTO {alias}.{schema}.m_child(id, parent_id) VALUES(12, 10)")
    rows, _ = pg.simple(f"SELECT code, qty::INTEGER FROM {alias}.{schema}.m_child WHERE id=12")
    assert rows == [["dflt", "1"]], f"{dialect}: default lost after SET/DROP NOT NULL: {rows}"

    # RETURNING behaviors
    if dialect == "postgres":
        r_upd, _ = pg.exec_typed(
            f"UPDATE {alias}.{schema}.m_child SET qty=$1 WHERE id=$2 RETURNING id", [23, 23], ["9", "10"]
        )
        assert r_upd == [["10"]], r_upd
        r_del, _ = pg.exec_typed(
            f"DELETE FROM {alias}.{schema}.m_child WHERE id=$1 RETURNING id", [23], ["10"]
        )
        assert r_del == [["10"]], r_del
    elif dialect == "mariadb":
        r_del, _ = pg.exec_typed(
            f"DELETE FROM {alias}.{schema}.m_child WHERE id=$1 RETURNING id", [23], ["10"]
        )
        assert r_del == [["10"]], f"MariaDB DELETE RETURNING failed: {r_del}"
        assert_err_sqlstate(
            lambda: pg.exec_typed(
                f"UPDATE {alias}.{schema}.m_child SET qty=$1 WHERE id=$2 RETURNING id", [23, 23], ["9", "11"]
            ),
            "0A000",
        )
    elif dialect == "mysql":
        assert_err_sqlstate(
            lambda: pg.exec_typed(
                f"DELETE FROM {alias}.{schema}.m_child WHERE id=$1 RETURNING id", [23], ["10"]
            ),
            "0A000",
        )
        assert_err_sqlstate(
            lambda: pg.exec_typed(
                f"UPDATE {alias}.{schema}.m_child SET qty=$1 WHERE id=$2 RETURNING id", [23, 23], ["9", "10"]
            ),
            "0A000",
        )

    # Conditional INDEX & SCHEMA idempotency
    for _ in range(2):
        pg.simple(f"CREATE INDEX IF NOT EXISTS m_idx ON {alias}.{schema}.m_child(qty)")
    for _ in range(2):
        pg.simple(f"DROP INDEX IF EXISTS {alias}.{schema}.m_idx")
    for _ in range(2):
        pg.simple(f"CREATE SCHEMA IF NOT EXISTS {alias}.{alt_schema}")
    for _ in range(2):
        pg.simple(f"DROP SCHEMA IF EXISTS {alias}.{alt_schema}")
    for _ in range(2):
        pg.simple(f"DROP TABLE IF EXISTS {alias}.{schema}.m_child")
        pg.simple(f"DROP TABLE IF EXISTS {alias}.{schema}.m_parent")
    print(f"[matrix] {dialect}: PASSED")


def main():
    binary = ROOT / "target" / "debug" / "examples" / "fedengine-test-host"
    if not binary.exists():
        subprocess.run(["cargo", "build", "--example", "fedengine-test-host", "--locked"], cwd=ROOT, check=True)
    libduckdb = find_duckdb_lib()
    ext_dir = (ROOT / "data" / "extensions").resolve()
    ext_versions = discover_extension_versions(ext_dir, libduckdb)

    images = {
        "pg": "postgres:17.7-alpine",
        "mysql": "mysql:5.7",
        "mariadb": "mariadb:10.11",
    }
    for img in images.values():
        ensure_cached_image(img)

    suffix = str(os.getpid())
    pw = "fedengine-matrix-only"
    containers: list[str] = []
    proc: subprocess.Popen | None = None
    try:
        pg_c = f"fedengine-matrix-pg-{suffix}"
        my_c = f"fedengine-matrix-mysql-{suffix}"
        mdb_c = f"fedengine-matrix-mariadb-{suffix}"
        for cmd, cname in [
            (
                ["docker", "run", "-d", "--rm", "--name", pg_c, "-p", f"127.0.0.1:{PG_PORT}:5432",
                 "-e", "POSTGRES_USER=fedengine", "-e", f"POSTGRES_PASSWORD={pw}", "-e", "POSTGRES_DB=crm",
                 images["pg"]],
                pg_c,
            ),
            (
                ["docker", "run", "-d", "--rm", "--name", my_c, "-p", f"127.0.0.1:{MY_PORT}:3306",
                 "-e", f"MYSQL_ROOT_PASSWORD={pw}", "-e", "MYSQL_DATABASE=erp",
                 images["mysql"]],
                my_c,
            ),
            (
                ["docker", "run", "-d", "--rm", "--name", mdb_c, "-p", f"127.0.0.1:{MDB_PORT}:3306",
                 "-e", f"MARIADB_ROOT_PASSWORD={pw}", "-e", "MARIADB_DATABASE=erp",
                 images["mariadb"]],
                mdb_c,
            ),
        ]:
            subprocess.run(cmd, stdout=subprocess.DEVNULL, check=True)
            containers.append(cname)

        wait_container_ready(pg_c, ["pg_isready", "-h", "127.0.0.1", "-U", "fedengine", "-d", "crm"])
        wait_container_ready(my_c, ["mysqladmin", "ping", "--protocol=TCP", "-h", "127.0.0.1", f"-p{pw}"])
        wait_container_ready(mdb_c, ["sh", "-c", f"mariadb-admin ping -h 127.0.0.1 -p{pw} || mysqladmin ping -h 127.0.0.1 -p{pw}"])

        pg_ver = subprocess.check_output(
            ["docker", "exec", pg_c, "psql", "-U", "fedengine", "-d", "crm", "-Atc", "SELECT current_setting('server_version')"],
            text=True,
        ).strip()
        my_ver = subprocess.check_output(
            ["docker", "exec", my_c, "mysql", "-N", "-B", "-h", "127.0.0.1", "-uroot", f"-p{pw}", "-e", "SELECT VERSION()"],
            text=True,
        ).strip()
        mdb_ver = subprocess.check_output(
            ["docker", "exec", mdb_c, "sh", "-c", f"(mariadb -N -B -h 127.0.0.1 -uroot -p{pw} -e 'SELECT VERSION()' 2>/dev/null || mysql -N -B -h 127.0.0.1 -uroot -p{pw} -e 'SELECT VERSION()')"],
            text=True,
        ).strip()

        with tempfile.TemporaryDirectory(prefix="fedengine-matrix-") as tmp:
            (Path(tmp) / "tmp").mkdir()
            (Path(tmp) / "secrets").mkdir()
            ops_toml = ", ".join(f'"{op}"' for op in ALL_OPERATIONS)
            cfg = f'''
[application]
name = "fedengine"
[pgwire]
listen = "127.0.0.1:0"
max_connections = 64
startup_timeout_ms = 10000
query_timeout_ms = 60000
output_timeout_ms = 10000
tls_required = false
max_statements = 128
max_portals = 64
allow_cross_database = true
[duckdb]
database_path = "{tmp}/meta.duckdb"
max_workers = 4
threads = 2
memory_limit = "1GB"
temp_directory = "{tmp}/tmp"
result_channel_batches = 8
batch_rows = 512
acquire_timeout_ms = 5000
install_extensions = false
extension_directory = "{ext_dir}"
pool_max_connections = 16
pool_wait_timeout_ms = 30000
[management]
enabled = false
listen = "127.0.0.1:0"
credentials_directory = "{tmp}/secrets"
queue_capacity = 64
max_concurrent_operations = 2
drain_timeout_ms = 30000
request_timeout_ms = 10000
[ddl]
enabled = true
allow_drop_schema = true
source_lock_timeout_ms = 30000
[shutdown]
grace_period_secs = 15
[[users]]
name = "fedengine"
password_secret_ref = "FEDENGINE_MATRIX_PASS"
[[datasources]]
id = "pg"
kind = "postgres"
catalog_alias = "pg_m"
secret_ref = "FEDENGINE_MATRIX_PG_DSN"
default_schema = "public"
read_only = false
ddl_enabled = true
allowed_schemas = ["public", "sales"]
[[datasources]]
id = "my"
kind = "mysql"
catalog_alias = "my_m"
secret_ref = "FEDENGINE_MATRIX_MY_DSN"
default_schema = "erp"
read_only = false
ddl_enabled = true
allowed_schemas = ["erp", "fed_ns"]
[[datasources]]
id = "mdb"
kind = "mariadb"
catalog_alias = "mdb_m"
secret_ref = "FEDENGINE_MATRIX_MDB_DSN"
default_schema = "erp"
read_only = false
ddl_enabled = true
allowed_schemas = ["erp", "fed_ns"]
[[grants]]
principal = "fedengine"
source = "pg"
schema = "*"
object = "*"
actions = ["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE_SCHEMA", "DROP_SCHEMA", "CREATE_TABLE", "CREATE_TABLE_AS", "ALTER_TABLE", "DROP_TABLE", "CREATE_INDEX", "DROP_INDEX"]
[[grants]]
principal = "fedengine"
source = "my"
schema = "*"
object = "*"
actions = ["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE_SCHEMA", "DROP_SCHEMA", "CREATE_TABLE", "CREATE_TABLE_AS", "ALTER_TABLE", "DROP_TABLE", "CREATE_INDEX", "DROP_INDEX"]
[[grants]]
principal = "fedengine"
source = "mdb"
schema = "*"
object = "*"
actions = ["SELECT", "INSERT", "UPDATE", "DELETE", "CREATE_SCHEMA", "DROP_SCHEMA", "CREATE_TABLE", "CREATE_TABLE_AS", "ALTER_TABLE", "DROP_TABLE", "CREATE_INDEX", "DROP_INDEX"]
[[capabilities]]
source = "pg"
duckdb_version = "v2.0.0"
extension_version = "{ext_versions["postgres_scanner"]}"
remote_version = "{pg_ver}"
evidence = "scripts/test-dialect-matrix.py"
operations = [{ops_toml}]
index_adapter = true
[[capabilities]]
source = "my"
duckdb_version = "v2.0.0"
extension_version = "{ext_versions["mysql_scanner"]}"
remote_version = "{my_ver}"
evidence = "scripts/test-dialect-matrix.py"
operations = [{ops_toml}]
index_adapter = true
[[capabilities]]
source = "mdb"
duckdb_version = "v2.0.0"
extension_version = "{ext_versions["mysql_scanner"]}"
remote_version = "{mdb_ver}"
evidence = "scripts/test-dialect-matrix.py"
operations = [{ops_toml}]
index_adapter = true
'''
            cfg_path = Path(tmp) / "matrix.toml"
            cfg_path.write_text(cfg)
            env = dict(
                os.environ,
                FEDENGINE_MATRIX_PASS=pw,
                FEDENGINE_MATRIX_PG_DSN=f"host=127.0.0.1 port={PG_PORT} dbname=crm user=fedengine password={pw}",
                FEDENGINE_MATRIX_MY_DSN=f"host=127.0.0.1 port={MY_PORT} database=erp user=root password={pw}",
                FEDENGINE_MATRIX_MDB_DSN=f"host=127.0.0.1 port={MDB_PORT} database=erp user=root password={pw}",
                LD_LIBRARY_PATH=str(libduckdb.parent) + os.pathsep + os.environ.get("LD_LIBRARY_PATH", ""),
                RUST_LOG="fedengine_test_host=info,modorch_fedengine=info,modorch_fedengine_duckdb=warn",
                NO_COLOR="1",
            )
            proc = subprocess.Popen(
                [str(binary), "--config", str(cfg_path)],
                cwd=ROOT,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            deadline = time.monotonic() + 30
            buffered = b""
            port = None
            while time.monotonic() < deadline:
                if proc.poll() is not None:
                    raise RuntimeError(f"fedengine exited early:\n{buffered.decode(errors='replace')}")
                if select.select([proc.stdout], [], [], 0.2)[0]:
                    buffered += os.read(proc.stdout.fileno(), 65536)
                    if b"FedEngine Ready" in buffered:
                        found = re.search(rb"127\.0\.0\.1:(\d+)", buffered)
                        if found:
                            port = int(found[1])
                            break
            if port is None:
                raise RuntimeError(f"timed out waiting for FedEngine Ready:\n{buffered.decode(errors='replace')}")

            client = PgClient("127.0.0.1", port, "fedengine", pw, "pg_m")
            try:
                run_dialect_checks(client, "pg_m", "public", "sales", "postgres")
                run_dialect_checks(client, "my_m", "erp", "fed_ns", "mysql")
                run_dialect_checks(client, "mdb_m", "erp", "fed_ns", "mariadb")
            finally:
                client.close()

            proc.send_signal(signal.SIGTERM)
            rest, _ = proc.communicate(timeout=20)
            assert proc.returncode == 0, f"fedengine shutdown failed:\n{(buffered + rest).decode(errors='replace')}"

            optional_odbc = [
                k for k in ("ORACLE", "SQLSERVER", "DAMENG", "DB2", "HANA")
                if os.environ.get(f"FEDENGINE_TEST_{k}_DSN")
            ]
            if optional_odbc:
                print(f"[matrix] optional ODBC env DSNs detected (live CTAS/RETURNING hook available): {', '.join(optional_odbc)}")
            print("[matrix] ALL EPHEMERAL DIALECT CHECKS PASSED")
    finally:
        if proc is not None and proc.poll() is None:
            proc.terminate()
            try:
                proc.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.communicate()
        for cname in containers:
            subprocess.run(["docker", "rm", "-fv", cname], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
