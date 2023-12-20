import errno
import os
import pytest
import signal
import re

from conftest import (
    Cluster,
    Instance,
    Retriable,
    TarantoolError,
    ReturnError,
    MalformedAPI,
)


def test_call_normalization(instance: Instance):
    assert instance.call("tostring", 1) == "1"
    assert instance.call("dostring", "return") is None
    assert instance.call("dostring", "return 1") == 1
    assert instance.call("dostring", "return { }") == []
    assert instance.call("dostring", "return 's'") == "s"
    assert instance.call("dostring", "return nil") is None
    assert instance.call("dostring", "return true") is True

    with pytest.raises(ReturnError) as e1:
        instance.call("dostring", "return nil, 'some error'")
    assert e1.value.args == ("some error",)

    with pytest.raises(MalformedAPI) as e2:
        instance.call("dostring", "return 'x', 1")
    assert e2.value.args == ("x", 1)

    with pytest.raises(TarantoolError) as e3:
        instance.call("error", "lua exception", 0)
    assert e3.value.args == ("ER_PROC_LUA", "lua exception")

    with pytest.raises(TarantoolError) as e4:
        instance.call("void")
    assert e4.value.args == ("ER_NO_SUCH_PROC", "Procedure 'void' is not defined")

    # Python connector for tarantool misinterprets timeout errors.
    # It should be TimeoutError instead of ECONNRESET
    with pytest.raises(OSError) as e5:
        instance.call("package.loaded.fiber.sleep", 1, timeout=0.1)
    assert e5.value.errno == errno.ECONNRESET

    with pytest.raises(OSError) as e6:
        instance.call("os.exit", 0)
    assert e6.value.errno == errno.ECONNRESET

    instance.terminate()
    with pytest.raises(OSError) as e7:
        instance.call("anything")
    assert e7.value.errno == errno.ECONNREFUSED


def test_eval_normalization(instance: Instance):
    assert instance.eval("return") is None
    assert instance.eval("return 1") == 1
    assert instance.eval("return { }") == []
    assert instance.eval("return 's'") == "s"
    assert instance.eval("return nil") is None
    assert instance.eval("return true") is True

    with pytest.raises(ReturnError) as e1:
        instance.eval("return nil, 'some error'")
    assert e1.value.args == ("some error",)

    with pytest.raises(MalformedAPI) as e2:
        instance.eval("return 'x', 2")
    assert e2.value.args == ("x", 2)

    with pytest.raises(TarantoolError) as e3:
        instance.eval("error('lua exception', 0)")
    assert e3.value.args == ("ER_PROC_LUA", "lua exception")

    with pytest.raises(TarantoolError) as e4:
        instance.eval("return box.schema.space.drop(0, 'void')")
    assert e4.value.args == ("ER_NO_SUCH_SPACE", "Space 'void' does not exist")


def test_process_management(instance: Instance):
    """
    The test ensures pytest can kill all subprocesses
    even if they don't terminate and hang
    """

    assert instance.eval("return 'ok'") == "ok"
    assert instance.process is not None
    pid = instance.process.pid
    pgrp = pid

    class StillAlive(Exception):
        pass

    def check_pg(pgrp):
        try:
            os.killpg(pgrp, 0)
        except ProcessLookupError:
            pass
        except PermissionError:
            # According to `man 2 kill`, MacOS raises it if at least one process
            # in the process group has insufficient permissions. In fact, it also
            # returns EPERM if the targed process is a zombie.
            # See https://git.picodata.io/picodata/picodata/picodata/-/snippets/7
            raise StillAlive
        else:
            raise StillAlive

    # Sigstop entire pg so that the picodata child can't
    # handle the supervisor termination
    os.killpg(pgrp, signal.SIGSTOP)

    # Now kill the supervisor
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)

    # Make sure the supervisor is dead
    with pytest.raises(ProcessLookupError):
        os.kill(pid, 0)

    # Make sure the child is still hanging
    with pytest.raises(OSError) as exc:
        instance.eval("return 'ok'", timeout=0.1)
    assert exc.value.errno == errno.ECONNRESET
    with pytest.raises(StillAlive):
        Retriable(timeout=1, rps=10).call(check_pg, pgrp)
    print(f"{instance} is still alive")

    # Kill the remaining child in the process group using conftest API
    instance.kill()

    # When the supervisor is killed, the orphaned child is reparented
    # to a subreaper. Pytest isn't the one, and therefore it can't do
    # `waitpid` directly. Instead, the test retries `killpg` until
    # it succeeds.
    #
    # Also, note, that after the child is killed, it remains
    # a zombie for a while. The child is removed from the process
    # table when a subreaper calls `waitpid`.
    #
    Retriable(timeout=1, rps=100).call(check_pg, pgrp)
    print(f"{instance} is finally dead")

    # Ensure the child is dead
    with pytest.raises(ProcessLookupError):
        os.killpg(pgrp, 0)

    # Check idempotency
    instance.start()
    pid1 = instance.process.pid
    instance.start()
    pid2 = instance.process.pid
    assert pid1 == pid2
    instance.terminate()
    instance.terminate()
    instance.kill()
    instance.kill()


def test_graceful_stop(instance: Instance):
    instance.terminate()
    *_, last_xlog = sorted(
        [f for f in os.listdir(instance.data_dir) if f.endswith(".xlog")]
    )
    with open(os.path.join(instance.data_dir, last_xlog), "rb") as f:
        assert f.read()[-4:] == b"\xd5\x10\xad\xed"


def test_whoami(instance: Instance):
    assert instance.call("pico.whoami") == {
        "raft_id": 1,
        "instance_id": "i1",
        "cluster_id": instance.cluster_id,
        "tier": "default",
    }


def test_whoami_in_different_tiers(cluster: Cluster):
    cfg = {
        "tier": {
            "storage": {"replication_factor": 1},
            "router": {"replication_factor": 2},
        }
    }

    cluster.set_init_cfg(cfg)
    i1 = cluster.add_instance(tier="storage")
    i2 = cluster.add_instance(tier="router")

    assert i1.call("pico.whoami") == {
        "raft_id": 1,
        "instance_id": "i1",
        "cluster_id": i1.cluster_id,
        "tier": "storage",
    }

    assert i2.call("pico.whoami") == {
        "raft_id": 2,
        "instance_id": "i2",
        "cluster_id": i2.cluster_id,
        "tier": "router",
    }


def test_instance_info(instance: Instance):
    def instance_info(iid: str | None = None):
        return instance.call("pico.instance_info", iid)

    # Don't compare entire structure, a couple of fields is enough
    myself = instance_info("i1")
    assert myself["raft_id"] == 1
    assert myself["instance_id"] == "i1"
    assert myself["replicaset_id"] == "r1"

    with pytest.raises(ReturnError) as e:
        instance_info("i2")
    assert e.value.args == ('instance with id "i2" not found',)

    assert instance_info() == myself


def test_raft_log(instance: Instance):
    raft_log = instance.call("pico.raft_log", dict(max_width=256))

    raft_log = str.join("\n", raft_log)

    def strip_spaces(s: str):
        s = s.strip()
        s = s.replace("\u200b", "")
        s = re.sub(r"[ ]*\|[ ]*", "|", s)
        s = re.sub(r"[-]*\+[-]*", "+", s)
        return s

    def space_id(space_name: str):
        return instance.eval("return box.space[...].id", space_name)

    expected = """\
+-----+----+-----+--------+
|index|term| lc  |contents|
+-----+----+-----+--------+
|  1  | 1  |1.0.1|Insert({_pico_peer_address}, [1,"127.0.0.1:{p}"])|
|  2  | 1  |1.0.2|Insert({_pico_instance}, ["i1","68d4a766-4144-3248-aeb4-e212356716e4",1,"r1","e0df68c5-e7f9-395f-86b3-30ad9e1b7b07",["Offline",0],["Offline",0],{b},"default"])|
|  3  | 1  |1.0.3|Insert({_pico_tier}, ["default",1])|
|  4  | 1  |1.0.4|Insert({_pico_property}, ["global_schema_version",0])|
|  5  | 1  |1.0.5|Insert({_pico_property}, ["next_schema_version",1])|
|  6  | 1  |1.0.6|Insert({_pico_property}, ["password_min_length",8])|
|  7  | 1  |1.0.7|Insert({_pico_property}, ["auto_offline_timeout",5.0])|
|  8  | 1  |1.0.8|Insert({_pico_property}, ["max_heartbeat_period",5.0])|
|  9  | 1  |1.0.9|Insert({_pico_property}, ["max_pg_portals",50])|
| 10  | 1  |1.0.10|Insert({_pico_property}, ["snapshot_chunk_max_size",16777216])|
| 11  | 1  |1.0.11|Insert({_pico_property}, ["snapshot_read_view_close_timeout",86400.0])|
| 12  | 1  |1.0.12|Insert({_pico_user}, [0,"guest",0,["chap-sha1","vhvewKp0tNyweZQ+cFKAlsyphfg="],1])|
| 13  | 1  |1.0.13|Insert({_pico_user}, [1,"admin",0,["chap-sha1",""],1])|
| 14  | 1  |1.0.14|Insert({_pico_role}, [2,"public",0,1])|
| 15  | 1  |1.0.15|Insert({_pico_role}, [31,"super",0,1])|
| 16  | 1  |1.0.16|Insert({_pico_privilege}, ["usage","universe",0,0,1,0])|
| 17  | 1  |1.0.17|Insert({_pico_privilege}, ["session","universe",0,0,1,0])|
| 18  | 1  |1.0.18|Insert({_pico_privilege}, ["usage","universe",0,1,1,0])|
| 19  | 1  |1.0.19|Insert({_pico_privilege}, ["session","universe",0,1,1,0])|
| 20  | 1  |1.0.20|Insert({_pico_privilege}, ["execute","role",2,0,1,0])|
| 21  | 1  |1.0.21|Insert({_pico_table}, [{_pico_table},"_pico_table",["global"],[["id","unsigned",false],["name","string",false],["distribution","array",false],["format","array",false],["schema_version","unsigned",false],["operable","boolean",false],["engine","string",false|
|  ~  | ~  |  ~   |],["owner","unsigned",false]],0,true,"memtx",1])|
| 22  | 1  |1.0.22|Insert({_pico_table}, [{_pico_index},"_pico_index",["global"],[["table_id","unsigned",false],["id","unsigned",false],["name","string",false],["local","boolean",false],["parts","array",false],["schema_version","unsigned",false],["operable","boolean",false],|
|  ~  | ~  |  ~   |["unique","boolean",false]],0,true,"memtx",1])|
| 23  | 1  |1.0.23|Insert({_pico_table}, [{_pico_peer_address},"_pico_peer_address",["global"],[["raft_id","unsigned",false],["address","string",false]],0,true,"memtx",1])|
| 24  | 1  |1.0.24|Insert({_pico_table}, [{_pico_instance},"_pico_instance",["global"],[["instance_id","string",false],["instance_uuid","string",false],["raft_id","unsigned",false],["replicaset_id","string",false],["replicaset_uuid","string",false],["current_grade","array",fals|
|  ~  | ~  |  ~   |e],["target_grade","array",false],["failure_domain","map",false],["tier","string",false]],0,true,"memtx",1])|
| 25  | 1  |1.0.25|Insert({_pico_table}, [{_pico_property},"_pico_property",["global"],[["key","string",false],["value","any",false]],0,true,"memtx",1])|
| 26  | 1  |1.0.26|Insert({_pico_table}, [{_pico_replicaset},"_pico_replicaset",["global"],[["replicaset_id","string",false],["replicaset_uuid","string",false],["master_id","string",false],["tier","string",false],["weight","number",false],["weight_origin","string",false],["state"|
|  ~  | ~  |  ~   |,"string",false]],0,true,"memtx",1])|
| 27  | 1  |1.0.27|Insert({_pico_table}, [{_pico_user},"_pico_user",["global"],[["id","unsigned",false],["name","string",false],["schema_version","unsigned",false],["auth","array",false],["owner","unsigned",false]],0,true,"memtx",1])|
| 28  | 1  |1.0.28|Insert({_pico_table}, [{_pico_privilege},"_pico_privilege",["global"],[["privilege","string",false],["object_type","string",false],["object_id","integer",false],["grantee_id","unsigned",false],["grantor_id","unsigned",false],["schema_version","unsigned",false]|
|  ~  | ~  |  ~   |],0,true,"memtx",1])|
| 29  | 1  |1.0.29|Insert({_pico_table}, [{_pico_role},"_pico_role",["global"],[["id","unsigned",false],["name","string",false],["schema_version","unsigned",false],["owner","unsigned",false]],0,true,"memtx",1])|
| 30  | 1  |1.0.30|Insert({_pico_table}, [{_pico_tier},"_pico_tier",["global"],[["name","string",false],["replication_factor","unsigned",false]],0,true,"memtx",1])|
| 31  | 1  |     |AddNode(1)|
| 32  | 2  |     |-|
| 33  | 2  |1.1.1|Replace({_pico_instance}, ["i1","68d4a766-4144-3248-aeb4-e212356716e4",1,"r1","e0df68c5-e7f9-395f-86b3-30ad9e1b7b07",["Offline",0],["Online",1],{b},"default"])|
| 34  | 2  |1.1.2|Insert({_pico_replicaset}, ["r1","e0df68c5-e7f9-395f-86b3-30ad9e1b7b07","i1","default",0.0,"auto","not-ready"])|
| 35  | 2  |1.1.3|Replace({_pico_instance}, ["i1","68d4a766-4144-3248-aeb4-e212356716e4",1,"r1","e0df68c5-e7f9-395f-86b3-30ad9e1b7b07",["Replicated",1],["Online",1],{b},"default"])|
| 36  | 2  |1.1.4|Update({_pico_replicaset}, ["r1"], [["=","weight",1.0], ["=","state","ready"]])|
| 37  | 2  |1.1.5|Replace({_pico_property}, ["target_vshard_config",[{{"e0df68c5-e7f9-395f-86b3-30ad9e1b7b07":[{{"68d4a766-4144-3248-aeb4-e212356716e4":["guest:@127.0.0.1:{p}","i1",true]}},1.0]}},"on"]])|
| 38  | 2  |1.1.6|Replace({_pico_property}, ["current_vshard_config",[{{"e0df68c5-e7f9-395f-86b3-30ad9e1b7b07":[{{"68d4a766-4144-3248-aeb4-e212356716e4":["guest:@127.0.0.1:{p}","i1",true]}},1.0]}},"on"]])|
| 39  | 2  |1.1.7|Replace({_pico_property}, ["vshard_bootstrapped",true])|
| 40  | 2  |1.1.8|Replace({_pico_instance}, ["i1","68d4a766-4144-3248-aeb4-e212356716e4",1,"r1","e0df68c5-e7f9-395f-86b3-30ad9e1b7b07",["Online",1],["Online",1],{b},"default"])|
+-----+----+-----+--------+
""".format(  # noqa: E501
        p=instance.port,
        b="{}",
        _pico_peer_address=space_id("_pico_peer_address"),
        _pico_property=space_id("_pico_property"),
        _pico_replicaset=space_id("_pico_replicaset"),
        _pico_instance=space_id("_pico_instance"),
        _pico_tier=space_id("_pico_tier"),
        _pico_privilege=space_id("_pico_privilege"),
        _pico_user=space_id("_pico_user"),
        _pico_role=space_id("_pico_role"),
        _pico_table=space_id("_pico_table"),
        _pico_index=space_id("_pico_index"),
    )
    assert strip_spaces(raft_log) == strip_spaces(expected)


def test_governor_notices_restarts(instance: Instance):
    def check_vshard_configured(instance: Instance):
        assert instance.eval(
            """
                local replicasets = vshard.router.info().replicasets
                local replica_uuid = replicasets[box.info.cluster.uuid].replica.uuid
                return replica_uuid == box.info.uuid
        """
        )

    # vshard is configured after first start
    check_vshard_configured(instance)

    assert instance.current_grade() == dict(variant="Online", incarnation=1)

    instance.restart()
    instance.wait_online()

    # vshard is configured again after restart
    check_vshard_configured(instance)

    assert instance.current_grade() == dict(variant="Online", incarnation=2)
