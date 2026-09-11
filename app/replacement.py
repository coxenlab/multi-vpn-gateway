"""通道容器替换。调用方持有 channel_state 的通道操作锁；日志不含 payload。"""
import logging
import time
import uuid
import docker.errors
from requests.exceptions import RequestException
import store
import registry
import replacement_store as journal
import replacement_docker as resources

LOG = logging.getLogger(__name__)


def _owner(record, restore=False):
    return resources.Owner(record.channel_id, record.operation_id + ('-restore' if restore else ''))


def _get(dc, name):
    try: return dc.containers.get(name)
    except docker.errors.NotFound: return None


def _identity(dc, identity):
    c = dc.containers.get(identity); c.reload()
    if c.id != identity: raise RuntimeError('容器 ID 与替换记录不匹配')
    return c


def _rename(dc, c, expected, target):
    c.reload()
    if c.attrs.get('Name') == '/' + target: return
    if c.attrs.get('Name') != '/' + expected: raise RuntimeError('容器名称已由外部改变')
    try: c.rename(target)
    except RequestException: pass
    c.reload()
    if c.attrs.get('Name') != '/' + target: raise RuntimeError('容器更名未确认')


def _policy(c, policy):
    try: c.update(restart_policy=policy)
    except RequestException: pass
    c.reload()
    actual = c.attrs.get('HostConfig', {}).get('RestartPolicy', {})
    if actual.get('Name') != policy.get('Name') or actual.get('MaximumRetryCount', 0) != policy.get('MaximumRetryCount', 0):
        raise RuntimeError('容器重启策略未确认')


def _running(c, running):
    c.reload()
    if c.attrs.get('State', {}).get('Running') is running: return
    try:
        if running: c.start()
        else: c.stop(timeout=10)
    except RequestException: pass
    c.reload()
    if c.attrs.get('State', {}).get('Running') is not running: raise RuntimeError('容器启停结果未确认')


def _remove(dc, c):
    identity = c.id
    try: c.remove(force=True)
    except RequestException: pass
    if _get(dc, identity) is not None: raise RuntimeError('容器清理未确认')


def _owned(dc, owner, canonical=False):
    names = [owner.candidate_name]
    if canonical: names.append('vpn-' + owner.channel)
    for name in names:
        c = _get(dc, name)
        if c is not None:
            if owner.owns(c.attrs.get('Config', {}).get('Labels'), 'candidate'): return c
            if name == owner.candidate_name: raise RuntimeError('候选容器归属不匹配')
    return None


def _remove_volume(dc, name, owner=None):
    try: volume = dc.volumes.get(name)
    except docker.errors.NotFound: return
    if owner and not owner.owns(volume.attrs.get('Labels'), 'candidate'): raise RuntimeError('候选数据卷归属不匹配')
    if dc.containers.list(all=True, filters={'volume': name}): raise RuntimeError('待清理数据卷仍被使用')
    try: volume.remove()
    except RequestException: pass
    try: dc.volumes.get(name)
    except docker.errors.NotFound: return
    raise RuntimeError('数据卷清理未确认')


def _advance(record, phase, **updates):
    payload = dict(record.payload, **updates)
    journal.advance(record, phase, payload)
    return journal.get(record.channel_id)


def _changed(ch, fields):
    changed = dict(ch)
    for k in ('name', 'server', 'username', 'ec_ver', 'probe_url'):
        if k in fields: changed[k] = store._clean_field(k, fields[k])
    if 'routing_enabled' in fields: changed['routing_enabled'] = fields['routing_enabled']
    return changed


def _initialize(manager, c, ch, config):
    spec = registry.get(ch['vpn_type'])
    manager.initialize_gui(c, spec)
    if spec.get('runtime') == 'oss': manager.oss_connect(c, spec, config)


def _runtime(c, volume, ch, connected=False, latency=None):
    c.reload()
    running = c.attrs.get('State', {}).get('Running') is True
    ports = c.ports.get('8080/tcp')
    return journal.AppliedRuntime(c.id, volume, int(ports[0]['HostPort']) if ports else None,
                                  'logged_in' if running and connected else ('running' if running else 'stopped'), latency if connected else None)


def _validate_source(c, cid, volume):
    if c.attrs.get('Name') != '/vpn-' + cid: raise RuntimeError('旧容器名称与通道不匹配')
    if not any(m.get('Name') == volume and m.get('Type') == 'volume' for m in c.attrs.get('Mounts', [])):
        raise RuntimeError('旧数据卷与容器不匹配')


def replace(ch, fields, force_start=False):
    import manager
    dc = manager.dc; cid = ch['id']
    queued = journal.get(cid)
    if queued and queued.phase != 'queued': raise RuntimeError('上一次修改尚未验证，请先完成登录或恢复上一次设置')
    fields = {**(queued.payload['fields'] if queued else {}), **fields}
    if registry.get(ch['vpn_type']).get('runtime') == 'byo' and ch.get('container_id'):
        raise RuntimeError('自装客户端的安装在原容器中，不能通过重建恢复或修改连接参数')
    old = _get(dc, ch['container_id']) if ch.get('container_id') else None
    volume = journal.data_volume(cid)
    if old is None and ch.get('container_id'): store.set_status(cid, 'error')
    if old is not None:
        if old.id != ch['container_id']: raise RuntimeError('旧容器 ID 与通道不匹配')
        _validate_source(old, cid, volume)
    elif _get(dc, 'vpn-' + cid) is not None:
        raise RuntimeError('正式容器名已被外部占用，保留现有资源')
    try: dc.volumes.get(volume); source_exists = True
    except docker.errors.NotFound: source_exists = False
    if old is not None and not source_exists: raise RuntimeError('旧数据卷不存在，保留原实例')
    desired = _changed(ch, fields)
    plan = manager.channel_plan(desired, ch.get('vnc_password', ''))
    # 在动旧容器之前确保两份镜像都已就绪；固定 ID，后续 tag 漂移不改变本次内容。
    try: selected = dc.images.get(plan['image'])
    except docker.errors.ImageNotFound: selected = dc.images.pull(plan['image'])
    plan['image'] = selected.id
    helper_image = dc.images.get('vpnmgr/oss-vpn:latest').id if source_exists else None
    old_config = store.get_config(cid); config = dict(old_config)
    for k in ('server', 'username', 'password'):
        if k in fields: config[k] = fields[k] if k == 'password' else store._clean_field(k, fields[k])
    old_fields = {k: (store.get_password(cid) if k == 'password' else ch.get(k, '')) for k in fields}
    old_fields = {k: ('' if v is None else v) for k, v in old_fields.items()}
    secrets = [i['key'] for i in registry.get(ch['vpn_type']).get('inputs', []) if i.get('secret')]
    operation = uuid.uuid4().hex
    owner = resources.Owner(cid, operation)
    payload = dict(fields=fields, old_fields=old_fields, old_config=old_config, config=config, secrets=secrets,
                   kind='replacement' if old is not None else 'initial', source_exists=source_exists,
                   old_id=old.id if old is not None else None, old_volume=volume, old_image=old.image.id if old is not None else None, new_image=selected.id,
                   old_running=old is not None and old.attrs.get('State', {}).get('Running') is True,
                   old_policy=old.attrs.get('HostConfig', {}).get('RestartPolicy', {'Name': 'no'}) if old is not None else {'Name': 'no'},
                   start=force_start or ch['status'] != 'stopped', config_applied=False)
    journal.begin(cid, operation, payload, queued=queued)
    try:
        record = journal.get(cid)
        resources.ensure_volume(dc, owner)
        plan['name'] = owner.candidate_name
        plan['restart_policy'] = {'Name': 'no'}
        resources.use_volume(plan, owner.volume_name)
        candidate = resources.create_candidate(dc, plan, owner)
        record = _advance(record, 'prepared', new_id=candidate)
        record = _advance(record, 'switching')
        if old is not None:
            _policy(old, {'Name': 'no'})
            _running(old, False)
            resources.copy_volume(dc, owner, old.id, volume, helper_image)
            _rename(dc, old, 'vpn-' + cid, 'vpn-' + cid + '-previous-' + operation)
        else:
            if _get(dc, 'vpn-' + cid) is not None: raise RuntimeError('正式容器名已被外部占用')
            if source_exists: resources.copy_unattached_volume(dc, owner, volume, helper_image)
        new = _identity(dc, candidate)
        _rename(dc, new, owner.candidate_name, 'vpn-' + cid)
        record = _advance(record, 'validating')
        connected, latency = False, None
        if payload['start']:
            _running(new, True)
            _initialize(manager, new, desired, config)
            if registry.get(ch['vpn_type']).get('runtime') == 'oss' or desired.get('login_method') == 'headless':
                for attempt in range(3):
                    connected, latency = manager.probe(desired)
                    if connected: break
                    if attempt < 2: time.sleep(2)
                if not connected: raise RuntimeError('新设置未通过原验证地址的 SOCKS 探活')
        waiting = not connected and (old is not None or source_exists or bool(fields))
        runtime = _runtime(new, owner.volume_name, desired, connected, latency)
        if payload['start'] and runtime.status == 'stopped': raise RuntimeError('候选容器在初始化时退出')
        journal.commit(record, fields, secrets, runtime, awaiting_login=waiting)
    except Exception as failure:
        # 提交结果不明必须先读回；已提交的新配置不能被异常处理再次回滚。
        record = journal.get(cid)
        if record and record.phase in ('awaiting_login', 'committed'):
            LOG.warning('通道 %s 替换已提交，后续收尾待重试', cid)
            return
        try: recover(cid)
        except Exception as rollback:
            raise RuntimeError('替换失败，旧资源已保留但恢复尚未完成，请使用恢复上一次设置') from rollback
        raise RuntimeError('替换失败，已恢复上一次设置: ' + str(failure)) from failure
    try:
        _policy(new, {'Name': 'unless-stopped'})
        if not waiting: cleanup(cid)
    except Exception:
        LOG.warning('通道 %s 已应用，旧资源清理待重试', cid)


def recover(cid):
    import manager
    dc = manager.dc; record = journal.get(cid)
    if record is None: return
    if record.phase == 'deleting': raise RuntimeError('通道删除已开始，请重试删除')
    if record.phase in ('committed', 'rolled_back'):
        cleanup(cid); return
    if record.phase != 'rolling_back':
        record = _advance(record, 'rolling_back', config_applied=record.phase == 'awaiting_login')
    p = record.payload; owner = _owner(record); canonical = 'vpn-' + cid
    # 未完成的复制容器必须先停，才能碰源或目标卷。
    helper = _get(dc, owner.copy_name)
    if helper:
        if not owner.owns(helper.attrs.get('Config', {}).get('Labels'), 'copy'): raise RuntimeError('复制容器归属不匹配')
        _remove(dc, helper)
    candidate = _owned(dc, owner, canonical=True)
    if candidate:
        _policy(candidate, {'Name': 'no'}); _running(candidate, False)
        if candidate.attrs.get('Name') == '/' + canonical: _rename(dc, candidate, canonical, owner.candidate_name)
    if p.get('kind') == 'initial':
        if _get(dc, canonical) is not None: raise RuntimeError('正式容器名已被外部占用，保留所有资源')
        if candidate: _remove(dc, candidate)
        journal.restore_absent(record, p['old_volume'], p['old_fields'] if p.get('config_applied') else None,
                               p['secrets'], stopped=not p['start'])
        cleanup(cid)
        return
    old = _identity(dc, p['old_id'])
    if old.attrs.get('Name') not in ('/' + canonical, '/' + canonical + '-previous-' + record.operation_id):
        raise RuntimeError('旧容器名称已变化，拒绝自动恢复')
    old_ch = _changed(store.get_channel(cid), p['old_fields'])
    if old.attrs.get('State', {}).get('Running') and old.attrs.get('Name') == '/' + canonical:
        restored = old # 准备失败，原运行实例未动。
    elif not p['old_running']:
        _running(old, False)
        _rename(dc, old, canonical + '-previous-' + record.operation_id, canonical)
        restored = old
    else:
        # hagb/oss 不可靠的原地 start 不用作恢复；保留旧实例，再用原镜像/卷新建。
        _policy(old, {'Name': 'no'}); _running(old, False)
        _rename(dc, old, canonical, canonical + '-previous-' + record.operation_id)
        recovery_owner = _owner(record, True)
        restored = _owned(dc, recovery_owner, canonical=True)
        if restored is not None:
            # 恢复阶段中断后，先读回并关闭本次恢复实例再重新建，避免重复注入启动第二个客户端。
            _remove(dc, restored); restored = None
        if restored is None:
            kw = manager.channel_plan(old_ch, old_ch.get('vnc_password', ''))
            kw.update(name=recovery_owner.candidate_name, image=p['old_image'], restart_policy={'Name': 'no'})
            resources.use_volume(kw, p['old_volume'])
            restored = dc.containers.get(resources.create_candidate(dc, kw, recovery_owner))
        _rename(dc, restored, recovery_owner.candidate_name, canonical)
        _running(restored, True)
        _initialize(manager, restored, old_ch, p['old_config'])
    runtime = _runtime(restored, p['old_volume'], old_ch)
    # 恢复仅宣称运行态；连通性仍由下一次真正 SOCKS 探活决定。
    journal.rolled_back(record, runtime, p['old_fields'] if p.get('config_applied') else None, p['secrets'])
    _policy(restored, p['old_policy'])
    cleanup(cid)


def cleanup(cid):
    import manager
    dc = manager.dc; record = journal.get(cid)
    if record is None: return
    if record.phase not in ('committed', 'rolled_back'): return
    ch = store.get_channel(cid)
    if record.payload.get('kind') == 'initial' and record.phase == 'rolled_back':
        if ch.get('container_id'): raise RuntimeError('无旧实例恢复后出现未知运行实例')
        owner = _owner(record)
        if _get(dc, 'vpn-' + cid) is not None: raise RuntimeError('正式容器名已被外部占用')
        extra = _owned(dc, owner)
        if extra: _remove(dc, extra)
        _remove_volume(dc, owner.volume_name, owner)
        journal.finish(cid, record.operation_id)
        return
    current = _identity(dc, ch['container_id'])
    if current.attrs.get('Name') != '/vpn-' + cid: raise RuntimeError('当前容器身份已变化')
    p = record.payload; owner = _owner(record)
    # 先确保当前容器确实持有已提交的数据卷，再移除多余实例。
    selected = journal.data_volume(cid)
    _validate_source(current, cid, selected)
    _policy(current, {'Name': 'unless-stopped'} if record.phase == 'committed' else p['old_policy'])
    old = _get(dc, p['old_id']) if p.get('old_id') else None
    if old and old.id != current.id:
        if old.attrs.get('Name') != '/vpn-' + cid + '-previous-' + record.operation_id: raise RuntimeError('旧容器名称已变化')
        if old.attrs.get('State', {}).get('Running'): raise RuntimeError('旧容器仍在运行')
        _remove(dc, old)
    for candidate_owner in (owner, _owner(record, True)):
        extra = _owned(dc, candidate_owner)
        if extra and extra.id != current.id: _remove(dc, extra)
    if record.phase == 'committed':
        # 原卷已在准备时核实；仍被其他容器使用时保留并报告，不强制删除。
        if p.get('source_exists', True): _remove_volume(dc, p['old_volume'])
    else:
        _remove_volume(dc, owner.volume_name, owner)
    journal.finish(cid, record.operation_id)


def confirmed(cid):
    """仅由同代次的真实成功探活、在操作锁内调用；重资源清理由后续维护完成。"""
    record = journal.get(cid)
    if record and record.phase == 'awaiting_login':
        if store.get_channel(cid)['container_id'] != record.payload['new_id']: raise RuntimeError('探活容器与候选代次不匹配')
        journal.confirm(record)


def recover_all():
    import channel_state
    import manager
    try: records = journal.records()
    except Exception:
        LOG.error('替换记录无法读取，保留所有资源等待恢复')
        return
    records = [record for record in records if record.phase != 'queued']
    for record in records:
        try:
            with channel_state.mutation(record.channel_id):
                if record.phase == 'awaiting_login': reconcile_waiting(record.channel_id)
                elif record.phase == 'deleting': discard(record.channel_id)
                else: recover(record.channel_id)
        except Exception:
            LOG.warning('通道 %s 有待恢复的替换操作，保留记录与数据', record.channel_id)
    if records: manager.rebuild()


def reconcile_waiting(cid):
    import manager
    record = journal.get(cid)
    if record is None or record.phase != 'awaiting_login': return
    owner = _owner(record); c = _owned(manager.dc, owner, canonical=True)
    if c is None: raise RuntimeError('待登录候选不存在，请恢复上一次设置')
    _rename(manager.dc, c, owner.candidate_name, 'vpn-' + cid)
    if not any(m.get('Name') == owner.volume_name for m in c.attrs.get('Mounts', [])): raise RuntimeError('候选数据卷已变化')
    record = _advance(record, 'awaiting_login', new_id=c.id)
    journal.resumed(record, _runtime(c, owner.volume_name, store.get_channel(cid)))
    if c.attrs.get('State', {}).get('Running'): _policy(c, {'Name': 'unless-stopped'})


def resume(cid):
    """等待登录期间继续使用新设置；不得把启动解释成丢弃新设置或清掉旧备份。"""
    import manager
    dc = manager.dc; record = journal.get(cid)
    if record is None: return False
    if record.phase == 'queued':
        replace(store.get_channel(cid), {}, force_start=True)
        return True
    if record.phase in ('committed', 'rolled_back'):
        cleanup(cid); return False
    if record.phase != 'awaiting_login': raise RuntimeError('上次操作尚未恢复，请先恢复上一次设置')
    owner = _owner(record); ch = store.get_channel(cid)
    c = _owned(dc, owner, canonical=True)
    if c is not None and c.attrs.get('State', {}).get('Running'):
        reconcile_waiting(cid); return True
    volume = dc.volumes.get(owner.volume_name)
    if not owner.owns(volume.attrs.get('Labels'), 'candidate'): raise RuntimeError('候选数据卷归属已变化')
    if c is None or c.attrs.get('State', {}).get('Status') != 'created':
        image = c.image.id if c else record.payload.get('new_image')
        if not image: raise RuntimeError('待登录候选缺少固定镜像，请恢复上一次设置')
        dc.images.get(image)
        kw = manager.channel_plan(ch, ch.get('vnc_password', ''))
        kw.update(name=owner.candidate_name, image=image, restart_policy={'Name': 'no'})
        resources.use_volume(kw, owner.volume_name)
        if c: _remove(dc, c)
        c = dc.containers.get(resources.create_candidate(dc, kw, owner))
    record = _advance(record, 'awaiting_login', new_id=c.id)
    _rename(dc, c, owner.candidate_name, 'vpn-' + cid)
    _running(c, True)
    _initialize(manager, c, ch, record.payload['config'])
    runtime = _runtime(c, owner.volume_name, ch)
    if runtime.status == 'stopped': raise RuntimeError('候选启动失败，旧资源已保留，可恢复上一次设置')
    journal.resumed(record, runtime)
    _policy(c, {'Name': 'unless-stopped'})
    return True


def before_stop(cid):
    """未提交的异常恢复也必须保持停用意图，不能为停止操作短暂登录旧客户端。"""
    import manager
    record = journal.get(cid)
    if record is None: return
    if record.phase == 'queued': return
    if record.phase == 'awaiting_login': return reconcile_waiting(cid)
    if record.phase == 'deleting': raise RuntimeError('通道删除已开始，请重试删除')
    if record.phase in ('committed', 'rolled_back'):
        cleanup(cid); return
    if record.payload.get('kind') == 'initial':
        _advance(record, record.phase, start=False)
        recover(cid)
        return
    old = _identity(manager.dc, record.payload['old_id'])
    if old.attrs.get('Name') not in ('/vpn-'+cid, '/vpn-'+cid+'-previous-'+record.operation_id): raise RuntimeError('旧容器名称已变化')
    if not any(m.get('Name') == record.payload['old_volume'] for m in old.attrs.get('Mounts', [])): raise RuntimeError('旧数据卷已变化')
    record = _advance(record, record.phase, old_running=False, start=False)
    _policy(old, {'Name': 'no'}); _running(old, False)
    recover(cid)


def discard(cid):
    """用户删除通道时先清全部关联实例；命名卷按现有删除策略保留。"""
    import manager
    dc = manager.dc; record = journal.get(cid)
    if record is None: return False
    if record.phase != 'deleting': journal.request_delete(record); record = journal.get(cid)
    owner = _owner(record); canonical = 'vpn-' + cid
    containers = {}
    old = _get(dc, record.payload['old_id']) if record.payload.get('old_id') else None
    if old:
        if old.attrs.get('Name') not in ('/'+canonical, '/'+canonical+'-previous-'+record.operation_id): raise RuntimeError('旧容器名称已变化')
        if not any(m.get('Name') == record.payload['old_volume'] for m in old.attrs.get('Mounts', [])): raise RuntimeError('旧数据卷已变化')
        containers[old.id] = old
    for candidate_owner in (owner, _owner(record, True)):
        c = _owned(dc, candidate_owner, canonical=True)
        if c: containers[c.id] = c
    current = _get(dc, canonical)
    if current and current.id not in containers: raise RuntimeError('正式容器名已被外部占用')
    helper = _get(dc, owner.copy_name)
    if helper:
        if not owner.owns(helper.attrs.get('Config', {}).get('Labels'), 'copy'): raise RuntimeError('复制容器归属不匹配')
        _remove(dc, helper)
    for c in containers.values():
        _policy(c, {'Name': 'no'}); _remove(dc, c)
    journal.deleted(record)
    return True
