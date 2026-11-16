#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""严格模拟 Tauri 2 IPC：按 Rust 命令签名校验 camelCase 键、按 capabilities 校验 ACL，
然后触发前端所有交互，收集错误。

依赖：系统 PATH 里有 chromium（无头模式）。运行：
    python3 scripts/verify_frontend_ipc.py
会临时在 dist/ 生成 mock 文件，结束后自动清理。
"""
import http.server
import json
import re
import socketserver
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / 'dist'
CONTRACT_FILE = Path(tempfile.gettempdir()) / 'toymdict-contract.json'
subprocess.run(
    [sys.executable, str(ROOT / 'scripts' / 'check_api_contract.py'),
     '--json-out=' + str(CONTRACT_FILE)], check=True)
CONTRACT = json.loads(CONTRACT_FILE.read_text(encoding='utf-8'))
CAP = json.loads((ROOT / 'src-tauri/capabilities/default.json').read_text(encoding='utf-8'))
PERMS = CAP['permissions']

# 真实 ACL：dialog:default 展开为 message/open/save；否则只认显式 allow-*
if 'dialog:default' in PERMS:
    ALLOWED_DIALOG = ['message', 'open', 'save']
else:
    ALLOWED_DIALOG = [p.split('allow-', 1)[1] for p in PERMS
                      if p.startswith('dialog:allow-')]
    # ask / confirm 是 message 的别名
    if 'ask' in ALLOWED_DIALOG or 'confirm' in ALLOWED_DIALOG:
        ALLOWED_DIALOG.append('message')

MOCK = r'''
(function () {
    'use strict';
    var CONTRACT = __CONTRACT__;
    var ALLOWED_DIALOG = __ALLOWED_DIALOG__;
    window.__CALLS__ = [];
    window.__ERRORS__ = [];
    window.__LISTENERS__ = {};
    window.__CONSOLE_ERRORS__ = [];
    var origErr = console.error;
    console.error = function () {
        window.__CONSOLE_ERRORS__.push(Array.prototype.join.call(arguments, ' '));
        origErr.apply(console, arguments);
    };
    window.__EMIT__ = function (ev, payload) {
        (window.__LISTENERS__[ev] || []).forEach(function (cb) {
            try { cb({ payload: payload }); }
            catch (e) { window.__CONSOLE_ERRORS__.push('emit ' + ev + ': ' + e.message); }
        });
    };
    window.confirm = function () {
        // 原生 confirm 被 Tauri 转成 plugin:dialog|message 命令
        window.__TAURI__.core.invoke('plugin:dialog|message', {})
            .catch(function (e) {
                window.__ERRORS__.push('ACL: ' + e.message);
                if (window.showError) window.showError('前端错误', e.message, 'error');
            });
        return true;
    };
    window.__TAURI__ = {
        core: {
            invoke: function (cmd, args) {
                args = args || {};
                window.__CALLS__.push({ cmd: cmd, args: args });
                if (cmd.indexOf('plugin:dialog|') === 0) {
                    var sub = cmd.split('|')[1];
                    if (ALLOWED_DIALOG.indexOf(sub) < 0) {
                        return Promise.reject(new Error('Command ' + cmd + ' not allowed by ACL'));
                    }
                    return Promise.resolve(null);
                }
                var need = CONTRACT[cmd];
                if (!need) return Promise.reject(new Error('command ' + cmd + ' not found'));
                for (var i = 0; i < need.length; i++) {
                    if (!(need[i] in args)) {
                        return Promise.reject(new Error(
                            'invalid args `' + need[i] + '` for command `' + cmd +
                            '`: command ' + cmd + ' missing required key ' + need[i]));
                    }
                }
                var ret = null;
                if (cmd === 'get_protocol_base') ret = 'http://mdx.localhost';
                else if (cmd === 'get_pending_errors') ret = [];
                else if (cmd === 'get_advanced_search_dicts') ret = [{ id: 'a', name: '测试词典' }];
                else if (cmd === 'add_group') ret = { ok: true, name: '测试分组' };
                else if (cmd === 'advanced_search') ret = { ok: true, total: 1, expanded: '南朝宋' };
                else if (cmd === 'cancel_advanced_search') ret = { ok: true };
                else if (cmd === 'open_devtools') ret = true;
                else if (cmd === 'get_global_hotkey_enabled') ret = true;
                else if (cmd === 'set_global_hotkey_enabled') ret = args.enabled;
                return Promise.resolve(ret);
            }
        },
        event: {
            listen: function (ev, cb) {
                (window.__LISTENERS__[ev] = window.__LISTENERS__[ev] || []).push(cb);
                return Promise.resolve(function () {});
            }
        }
    };
})();
'''.replace('__CONTRACT__', json.dumps(CONTRACT, ensure_ascii=False)).replace(
    '__ALLOWED_DIALOG__', json.dumps(ALLOWED_DIALOG))

HARNESS = r'''
(function () {
    'use strict';
    var errors = [];
    var oldShowError = window.showError;
    window.showError = function (title, msg, level) {
        errors.push({ title: title, msg: String(msg), level: level });
        if (oldShowError) { try { oldShowError(title, msg, level); } catch (e) {} }
    };
    window.addEventListener('unhandledrejection', function (e) {
        errors.push({ title: 'unhandledrejection', msg: String(e.reason && e.reason.message || e.reason) });
    });

    function step(label, fn) {
        try { fn(); }
        catch (e) { errors.push({ title: 'harness', msg: label + ': ' + e.message }); }
    }
    function run() {
        var A = window.pywebview.api;
        var id = 'D:\\mdx\\test.mdx';
        // 1. 搜索与结果显示
        document.getElementById('searchInput').value = '南朝宋';
        step('triggerSearch', triggerSearch);
        step('showEntry', function () { showEntry(0); });
        step('show_entry 命令', function () { A.show_entry(0); });
        // 2. 分组与词典管理
        step('switch_group', function () { A.switch_group('默认分组'); });
        step('init_group_view', function () { A.init_group_view(); });
        step('add_group', function () { A.add_group('测试分组'); });
        step('add_dict_to_group', function () { A.add_dict_to_group(id); });
        step('remove_dict_from_group', function () { A.remove_dict_from_group(id); });
        step('exclude_dict', function () { A.exclude_dict(id); });
        step('reload_excluded_dict', function () { A.reload_excluded_dict(id); });
        step('move_dict', function () { A.move_dict(id, 'up'); });
        step('get_dict_info', function () { A.get_dict_info(id); });
        step('deleteCurrentGroup', deleteCurrentGroup);   // 内含 confirm() -> dialog ACL
        step('open_file', function () { A.open_file(); });
        step('open_folder', function () { A.open_folder(); });
        // 3. 高级搜索
        _advState.dicts = [{ id: 'a', name: '测试词典' }];
        _advState.selected = { a: true };
        document.getElementById('advPattern').value = '南朝宋';
        step('openAdvancedSearch', openAdvancedSearch);
        step('startAdvancedSearch', startAdvancedSearch);
        step('cancel_advanced_search', function () { A.cancel_advanced_search(); });
        // 4. 后端 evaluate_js 等价通道
        step('emit updateUI', function () {
            window.__EMIT__('js_call', { fn: 'updateUI', args: [{ groups: [{ name: '默认分组' }], current: '默认分组' }] });
        });
        step('emit renderGroupView', function () {
            window.__EMIT__('js_call', {
                fn: 'renderGroupView',
                args: [[], [{ name: '默认分组', dicts: [] }], '默认分组']
            });
        });
        step('emit clearAdvancedResults', function () {
            window.__EMIT__('js_call', { fn: 'clearAdvancedResults', args: ['南朝宋'] });
        });
        step('emit updateResults', function () {
            window.__EMIT__('js_call', { fn: 'updateResults', args: [[{ key: '南朝宋', sources: [] }]] });
        });
        // 4b-2. 词条 iframe 高度自适应：iframe 以 0 高起步，高度由词条页
        //        ResizeObserver 上报（零面积 iframe 的 rAF 可能被 Chromium
        //        节流，父窗口 load/看门狗会补发 calcHeight 兜底）。这里
        //        验证 resize 消息能把 iframe 撑开、calcHeight 可送达不抛错。
        step('词条 iframe 高度自适应', function () {
            setContent([{ dict_name: '高度测试', url: 'about:blank' }]);
            var f = document.querySelector('#contentArea iframe[id^="dict-iframe-"]');
            window.__HEIGHT_ZERO__ = !!(f && parseInt(f.style.height, 10) === 0);
            if (f) {
                // 真实词条 iframe 的 postMessage 会携带自己的 origin；app.js
                // 的 isTrustedMessage 要求 event.origin === TOYMDICT_ORIGIN，
                // 所以这里必须带上正确 origin，否则是假阴性（消息被安全校验
                // 丢弃，看起来就像「条目无法展开」）。
                window.dispatchEvent(new MessageEvent('message', {
                    data: { type: 'resize', id: f.id, height: 460 },
                    origin: window.TOYMDICT_ORIGIN || 'http://mdx.localhost'
                }));
                window.__HEIGHT_SET__ = parseInt(f.style.height, 10) === 460;
                try { f.contentWindow.postMessage('calcHeight', '*'); } catch (e) {}
                var block = f.closest('details');
                if (block && block.parentNode) block.parentNode.removeChild(block);
            }
        });
        // 4a. 跨域词条 iframe 的选区回传：主文档没有选区时，桥接层会向 iframe
        //     广播 toymdict-get-selection，这里模拟 iframe 内脚本的 postMessage 回复。
        step('跨域 iframe 选区回传', function () {
            window.__lastLocalCapture = 0;
            var b0 = document.getElementById('searchInput');
            if (b0) { b0.value = ''; b0.setSelectionRange(0, 0); }
            try { document.activeElement.blur(); } catch (e) {}
            try { window.getSelection().removeAllRanges(); } catch (e) {}
            var f = document.createElement('iframe');
            f.id = '__capture_frame__';
            f.src = 'about:blank';
            document.body.appendChild(f);
            window.__EMIT__('js_call', { fn: 'captureInAppSelection', args: [] });
            window.dispatchEvent(new MessageEvent('message', {
                data: { type: 'toymdict-selection', text: '跨域词条' }
            }));
            window.__CROSS_FRAME_OK__ =
                document.getElementById('searchInput').value === '跨域词条';
            var b = document.getElementById('searchInput');
            if (b) b.value = '';
            f.parentNode.removeChild(f);
        });
        // 4b. 应用内选词：构造一个真实选区，模拟后端在「应用内双击 Ctrl」时
        //     推送的 captureInAppSelection，验证选区文字被写进搜索框并触发搜索。
        step('应用内选词 captureInAppSelection', function () {
            window.__lastLocalCapture = 0;
            try { document.activeElement.blur(); } catch (e) {}
            var box0 = document.getElementById('searchInput');
            if (box0) { box0.value = ''; box0.setSelectionRange(0, 0); }
            var span = document.createElement('span');
            span.id = '__capture_test__';
            span.textContent = '南朝宋';
            document.body.appendChild(span);
            var range = document.createRange();
            range.selectNodeContents(span);
            var sel = window.getSelection();
            sel.removeAllRanges();
            sel.addRange(range);
            window.__EMIT__('js_call', { fn: 'captureInAppSelection', args: [] });
        });
        // 4c. 前端本地双击 Ctrl 取词：主文档自己判定手势并读选区，
        //     完全不依赖后端钩子——焦点在主窗口时这条通道最先生效。
        step('前端本地双击 Ctrl 取词', function () {
            window.__lastLocalCapture = 0;
            var box0 = document.getElementById('searchInput');
            if (box0) { box0.value = ''; box0.setSelectionRange(0, 0); }
            try { document.activeElement.blur(); } catch (e) {}
            var span = document.createElement('span');
            span.id = '__local_ctrl_test__';
            span.textContent = '本地取词';
            document.body.appendChild(span);
            var range = document.createRange();
            range.selectNodeContents(span);
            var sel = window.getSelection();
            sel.removeAllRanges();
            sel.addRange(range);
            function down() {
                document.dispatchEvent(new KeyboardEvent('keydown',
                    { key: 'Control', bubbles: true, cancelable: true }));
            }
            function up() {
                document.dispatchEvent(new KeyboardEvent('keyup',
                    { key: 'Control', bubbles: true, cancelable: true }));
            }
            down(); up(); down(); up();
            window.__LOCAL_CTRL_OK__ =
                document.getElementById('searchInput').value === '本地取词' &&
                window.__lastLocalCapture > 0;
            sel.removeAllRanges();
            span.parentNode.removeChild(span);
            document.getElementById('searchInput').value = '南朝宋';
        });
        // 4d. 词条 iframe 自己判定双击 Ctrl 后主动上报选区。
        step('词条 iframe 主动上报选区', function () {
            window.__lastLocalCapture = 0;
            var b = document.getElementById('searchInput');
            if (b) b.value = '';
            window.dispatchEvent(new MessageEvent('message', {
                data: { type: 'toymdict-hotkey-selection', text: '跨域词条' }
            }));
            window.__FRAME_HOTKEY_OK__ =
                document.getElementById('searchInput').value === '跨域词条';
            if (b) b.value = '南朝宋';
        });
        // 4e. 后端剪贴板通道的落点：本地刚取过词时去重，过后正常生效。
        step('externalSearchFromHotkey 去重', function () {
            window.__lastLocalCapture = Date.now();
            var b = document.getElementById('searchInput');
            if (b) b.value = '南朝宋';
            window.__EMIT__('js_call', { fn: 'externalSearchFromHotkey', args: ['剪贴板词'] });
            window.__DEDUP_OK__ =
                document.getElementById('searchInput').value === '南朝宋';
        });
        step('externalSearchFromHotkey 生效', function () {
            window.__lastLocalCapture = 0;
            window.__EMIT__('js_call', { fn: 'externalSearchFromHotkey', args: ['剪贴板词'] });
            window.__HOTKEY_OK__ =
                document.getElementById('searchInput').value === '剪贴板词';
            var b = document.getElementById('searchInput');
            if (b) b.value = '南朝宋';
        });
        // 5. 全局快捷键（含 DevTools 与系统级取词开关）
        step('open_devtools 命令', function () { A.open_devtools(); });
        step('取词开关命令', function () {
            A.get_global_hotkey_enabled();
            A.set_global_hotkey_enabled(true);
        });
        function press(k, opts) {
            var o = Object.assign({ key: k, bubbles: true, cancelable: true }, opts || {});
            document.dispatchEvent(new KeyboardEvent('keydown', o));
        }
        step('快捷键 F12', function () { press('F12'); });
        step('快捷键 Ctrl+Shift+I', function () { press('I', { ctrlKey: true, shiftKey: true }); });
        step('快捷键 Ctrl+F', function () { press('f', { ctrlKey: true }); });
        step('快捷键 Ctrl+Shift+F', function () { press('F', { ctrlKey: true, shiftKey: true }); });
        step('快捷键 Ctrl+G', function () { press('g', { ctrlKey: true }); });
        step('快捷键 Ctrl+/', function () { press('/', { ctrlKey: true }); });
        step('快捷键 Ctrl+Shift+H', function () { press('H', { ctrlKey: true, shiftKey: true }); });
        // 6. 词典信息「描述」按 Python 版 <iframe srcdoc> 渲染：后端把描述经
        //    html.escape(quote=True) 属性转义后塞进 srcdoc，浏览器解析属性时
        //    解回原始标签，iframe 内应真正渲染出 <font> / <br> 而非尖括号源码。
        step('词典信息描述 iframe srcdoc 渲染', function () {
            var info = '<p><b>文件路径:</b> <code>x.mdx</code></p>'
                + "<p style='margin-top:8px;'><b>描述:</b></p>"
                + '<iframe srcdoc="&lt;font color=&quot;red&quot;&gt;汉语大字典&lt;/font&gt;&lt;br/&gt;第二行" '
                + "style='width:100%;height:150px;border:1px solid #e0e0e0;border-radius:4px;background:#fff;'></iframe>";
            window.__EMIT__('js_call', { fn: 'showDictInfoModal', args: ['测试词典', info] });
        });

        setTimeout(function () {
            var calls = window.__CALLS__ || [];
            var cmds = calls.map(function (c) { return c.cmd; });
            var bad = errors.filter(function (e) {
                return /missing required key|not allowed by ACL|not found/.test(e.msg);
            });
            var missing = [];
            Object.keys(CONTRACT_KEYS).forEach(function (c) {
                if (cmds.indexOf(c) < 0) missing.push(c);
            });
            var dual = calls.filter(function (c) {
                return c.cmd === 'advanced_search' || c.cmd === 'search' || c.cmd === 'add_dict_to_group';
            });
            var result = {
                total_calls: calls.length,
                unique_cmds: Array.from(new Set(cmds)).sort(),
                param_errors: bad,
                acl_errors: errors.filter(function (e) { return /ACL/.test(e.title) || /ACL/.test(e.msg); }),
                all_errors: errors,
                console_errors: window.__CONSOLE_ERRORS__ || [],
                js_call_listeners: (window.__LISTENERS__['js_call'] || []).length,
                group_select_options: document.getElementById('groupSelect').options.length,
                in_app_capture_fn: typeof window.captureInAppSelection === 'function',
                in_app_capture_value: (document.getElementById('searchInput') || {}).value || '',
                cross_frame_capture: !!window.__CROSS_FRAME_OK__,
                entry_height_zero: !!window.__HEIGHT_ZERO__,
                entry_height_set: !!window.__HEIGHT_SET__,
                external_search_from_hotkey_fn: typeof window.externalSearchFromHotkey === 'function',
                local_double_ctrl: !!window.__LOCAL_CTRL_OK__,
                frame_hotkey_selection: !!window.__FRAME_HOTKEY_OK__,
                hotkey_dedup: !!window.__DEDUP_OK__,
                hotkey_search: !!window.__HOTKEY_OK__,
                missing_commands: missing,
                auto_search_on_switch: (function () {
                    var i = cmds.indexOf('switch_group');
                    return i >= 0 && cmds.indexOf('search', i + 1) >= 0;
                })(),
                devtools_calls: cmds.filter(function (c) { return c === 'open_devtools'; }).length,
                dual_key_samples: dual.map(function (c) { return { cmd: c.cmd, keys: Object.keys(c.args) }; }),
                dict_info_desc: (function () {
                    var box = document.getElementById('dictInfoBody');
                    var f = box && box.querySelector('iframe[srcdoc]');
                    if (!f) return { ok: false, why: 'no iframe' };
                    var attr = f.getAttribute('srcdoc') || '';
                    var d = null;
                    try { d = f.contentDocument; } catch (e) {}
                    var br = !!(d && d.querySelector('br'));
                    var txt = d && d.body ? d.body.textContent : '';
                    return {
                        ok: attr.indexOf('<font') >= 0 && br && txt.indexOf('第二行') >= 0,
                        attr_decoded: attr.indexOf('<font') >= 0,
                        br_rendered: br,
                        text_ok: txt.indexOf('第二行') >= 0
                    };
                })(),
                dialog_allowed: ALLOWED_DIALOG
            };
            var pre = document.createElement('pre');
            pre.id = 'TESTOUT';
            pre.textContent = JSON.stringify(result);
            document.body.appendChild(pre);
        }, 1200);
    }
    window.addEventListener('load', function () { setTimeout(run, 300); });
})();
'''.replace('CONTRACT_KEYS', json.dumps(CONTRACT, ensure_ascii=False)).replace(
    'ALLOWED_DIALOG', json.dumps(ALLOWED_DIALOG))


def main():
    (DIST / '__mock__.js').write_text(MOCK, encoding='utf-8')
    (DIST / '__harness__.js').write_text(HARNESS, encoding='utf-8')
    html = (DIST / 'index.html').read_text(encoding='utf-8')
    html = html.replace('<script src="tauri-bridge.js"></script>',
                        '<script src="__mock__.js"></script>\n'
                        '    <script src="tauri-bridge.js"></script>')
    html = html.replace('<script src="app.js"></script>',
                        '<script src="app.js"></script>\n'
                        '    <script src="__harness__.js"></script>')
    (DIST / 'test_index.html').write_text(html, encoding='utf-8')

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *a):
            pass

    class Server(socketserver.TCPServer):
        allow_reuse_address = True

    import functools
    srv = Server(
        ('127.0.0.1', 8933), functools.partial(Quiet, directory=str(DIST)))
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    try:
        r = subprocess.run([
            'chromium', '--headless', '--no-sandbox', '--disable-gpu',
            '--disable-dev-shm-usage', '--virtual-time-budget=10000',
            '--dump-dom', 'http://127.0.0.1:8933/test_index.html'],
            capture_output=True, text=True, timeout=120)
        m = re.search(r'<pre id="TESTOUT">(.*?)</pre>', r.stdout, re.S)
        if not m:
            print('NO TESTOUT (dom len=%d)' % len(r.stdout))
            print(r.stdout[-2000:])
            print('STDERR:', r.stderr[-1000:])
            return 2
        import html as H
        data = json.loads(H.unescape(m.group(1)))
    finally:
        srv.shutdown()
        for f in ('__mock__.js', '__harness__.js', 'test_index.html'):
            (DIST / f).unlink(missing_ok=True)

    ok = True
    def check(label, cond, detail=''):
        nonlocal ok
        print(('  ✔ ' if cond else '  ✖ ') + label + (('  ' + str(detail)) if detail else ''))
        if not cond:
            ok = False

    print('ACL 允许的 dialog 命令:', data['dialog_allowed'])
    print('IPC 调用 %d 次，覆盖 %d 个命令' % (data['total_calls'], len(data['unique_cmds'])))
    check('无参数键缺失错误', len(data['param_errors']) == 0, data['param_errors'])
    check('无 ACL 拒绝错误', len(data['acl_errors']) == 0, data['acl_errors'])
    check('无其它前端错误弹窗', len(data['all_errors']) == 0, data['all_errors'])
    check('js_call 监听已注册', data['js_call_listeners'] >= 1, data['js_call_listeners'])
    check('后端推送 updateUI 已渲染分组', data['group_select_options'] > 0,
          data['group_select_options'])
    check('所有命令均被调用', len(data['missing_commands']) == 0, data['missing_commands'])
    check('F12 / Ctrl+Shift+I 快捷键打开 DevTools', data['devtools_calls'] >= 3,
          data['devtools_calls'])
    check('切换分组后自动触发搜索', bool(data.get('auto_search_on_switch')),
          data.get('auto_search_on_switch'))
    check('应用内选词入口已注册', bool(data.get('in_app_capture_fn')),
          data.get('in_app_capture_fn'))
    check('应用内选词写入搜索框并触发搜索', data.get('in_app_capture_value') == '南朝宋',
          data.get('in_app_capture_value'))
    check('跨域词条 iframe 选区经 postMessage 回传', bool(data.get('cross_frame_capture')),
          data.get('cross_frame_capture'))
    check('词条 iframe 以 0 高起步（高度由词条页上报）', bool(data.get('entry_height_zero')),
          data.get('entry_height_zero'))
    check('词条 iframe 高度自适应（resize 消息撑开 + calcHeight 可送达）',
          bool(data.get('entry_height_set')), data.get('entry_height_set'))
    check('后端剪贴板取词入口已注册', bool(data.get('external_search_from_hotkey_fn')),
          data.get('external_search_from_hotkey_fn'))
    check('前端本地双击 Ctrl 取词生效', bool(data.get('local_double_ctrl')),
          data.get('local_double_ctrl'))
    check('词条 iframe 主动上报选区生效', bool(data.get('frame_hotkey_selection')),
          data.get('frame_hotkey_selection'))
    check('双通道取词去重生效', bool(data.get('hotkey_dedup')),
          data.get('hotkey_dedup'))
    check('后端剪贴板取词在去重窗口后生效', bool(data.get('hotkey_search')),
          data.get('hotkey_search'))
    _desc = data.get('dict_info_desc') or {}
    check('词典信息描述按 iframe srcdoc 渲染（与 Python 版一致）',
          bool(_desc.get('ok')), _desc)
    check('无 js_call 执行异常', len(data['console_errors']) == 0, data['console_errors'])
    print('双键样例:')
    for s in data['dual_key_samples'][:4]:
        print('   ', s['cmd'], s['keys'])
    print('\n结果:', '全部通过' if ok else '存在失败项')
    return 0 if ok else 1


if __name__ == '__main__':
    raise SystemExit(main())
