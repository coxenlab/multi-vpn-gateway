"""桌面写入的停止意图与 Web 共用；确认实例停止后才清除。"""
import uuid
import docker
import store


def ticket(cid):
    with store._c() as c:
        row=c.execute('SELECT operation_id FROM channel_stop_intents WHERE channel_id=?',(cid,)).fetchone()
    return row[0] if row else None


def pending(cid): return ticket(cid) is not None


def request(cid):
    with store._c() as c:
        c.execute('BEGIN IMMEDIATE')
        row=c.execute('SELECT container_id FROM channels WHERE id=?',(cid,)).fetchone()
        if row is None: raise RuntimeError('通道不存在')
        phase=c.execute('SELECT phase FROM channel_replacements WHERE channel_id=?',(cid,)).fetchone()
        if phase and phase[0]=='deleting': raise RuntimeError('通道删除已开始，请重试删除')
        if row[0] or phase:
            c.execute('INSERT OR IGNORE INTO channel_stop_intents(channel_id,operation_id) VALUES(?,?)',(cid,uuid.uuid4().hex))
        c.execute("UPDATE channels SET status='stopped',latency_ms=NULL WHERE id=?",(cid,))


def confirmed(cid,operation):
    with store._c() as c:
        c.execute('BEGIN IMMEDIATE')
        if c.execute('DELETE FROM channel_stop_intents WHERE channel_id=? AND operation_id=?',(cid,operation)).rowcount!=1:
            raise RuntimeError('停止操作代次已变化')
        if c.execute("UPDATE channels SET status='stopped',latency_ms=NULL WHERE id=?",(cid,)).rowcount!=1:
            raise RuntimeError('通道不存在')


def _get(dc,identity):
    try: return dc.containers.get(identity)
    except docker.errors.NotFound: return None


def apply(cid):
    import manager, replacement
    operation=ticket(cid)
    if operation is None: return
    replacement.before_stop(cid)
    channel=store.get_channel(cid)
    if channel is None: raise RuntimeError('通道不存在')
    canonical='vpn-'+cid; expected=channel.get('container_id')
    current=_get(manager.dc,canonical)
    if current:
        if not expected or current.id!=expected or current.attrs.get('Name')!='/'+canonical:
            raise RuntimeError('通道实例身份已变化，停止意图已保留')
        if current.attrs.get('State',{}).get('Running') is not False:
            try: current.stop()
            except Exception: pass  # 只读回确认，不重放 stop
        after=_get(manager.dc,expected)
        if after and (after.id!=expected or after.attrs.get('State',{}).get('Running') is not False):
            raise RuntimeError('通道停止尚未确认')
    elif expected and _get(manager.dc,expected) is not None:
        raise RuntimeError('原实例已改名，停止意图已保留')
    confirmed(cid,operation)


def recover_all():
    import channel_state
    for channel in store.list_channels():
        with channel_state.mutation(channel['id']): apply(channel['id'])
