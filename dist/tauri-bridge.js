/* pywebview.api -> Tauri invoke 兼容层。
 * 现有 app.js 无需改动：所有 pywebview.api.xxx(...) 调用被代理成 invoke(xxx, 具名参数)，
 * 后端所有 evaluate_js 调用被转成 js_call 事件，再分派到同名全局函数。 */
(function () {
    'use strict';

    function invoke(cmd, args) {
        var core = window.__TAURI__ && window.__TAURI__.core;
        if (!core || !core.invoke) {
            return Promise.reject(new Error('Tauri 运行时未就绪'));
        }
        return core.invoke(cmd, args || {});
    }

    // Python 版用位置参数，Tauri 用具名参数；这里按方法名还原参数名。
    var ARG_NAMES = {
        open_file: [],
        open_folder: [],
        get_pending_errors: [],
        get_advanced_search_dicts: [],
        get_protocol_base: [],
        open_devtools: [],
        get_global_hotkey_enabled: [],
        set_global_hotkey_enabled: ['enabled'],
        init_group_view: [],
        delete_group: [],
        cancel_advanced_search: [],
        switch_group: ['name'],
        add_group: ['name'],
        add_dict_to_group: ['dict_id'],
        remove_dict_from_group: ['dict_id'],
        exclude_dict: ['dict_id'],
        reload_excluded_dict: ['dict_id'],
        get_dict_info: ['dict_id'],
        show_entry: ['index'],
        search: ['keyword', 'use_variants'],
        move_dict: ['dict_id', 'action'],
        advanced_search: ['pattern', 'scope', 'use_variants', 'dict_ids']
    };

    // Tauri 2 的 #[tauri::command] 宏默认把 Rust 的 snake_case 参数名转成
    // camelCase 作为 JS 侧键名（dict_id -> dictId、use_variants -> useVariants）。
    // 后端命令仍沿用 Python 版的 snake_case 命名，因此这里同时发两种键：
    // Tauri 的 CommandItem 用 payload.get(key) 逐个取值，多余键会被忽略，
    // 所以双写对任何 Tauri 版本都安全，且不会因版本升级再次失效。
    function toCamel(s) {
        return s.replace(/_([a-z])/g, function (_, c) { return c.toUpperCase(); });
    }

    function toArgs(name, args) {
        var names = ARG_NAMES[name] || [];
        var obj = {};
        for (var i = 0; i < names.length; i++) {
            var snake = names[i];
            var camel = toCamel(snake);
            obj[snake] = args[i];
            if (camel !== snake) obj[camel] = args[i];
        }
        return obj;
    }

    window.pywebview = {
        api: new Proxy({}, {
            get: function (_, name) {
                if (typeof name !== 'string') return undefined;
                // 切换分组后用当前搜索框关键词自动重搜一次。必须等后端把
                // current_group 写完再触发，否则 search 读到的还是旧分组；
                // 搜索框为空时 triggerSearch() 自身会直接返回，不做多余搜索。
                if (name === 'switch_group') {
                    return function (group) {
                        return invoke('switch_group', toArgs('switch_group', [group]))
                            .then(function () {
                                try {
                                    if (typeof window.triggerSearch === 'function') {
                                        window.triggerSearch();
                                    }
                                } catch (err) {
                                    console.error('切换分组后自动搜索失败', err);
                                }
                            });
                    };
                }
                return function () {
                    return invoke(name, toArgs(name, Array.prototype.slice.call(arguments)));
                };
            }
        })
    };

    // ------------------------------------------------------------------ 全局快捷键
    // DevTools 能力由 tauri 的 devtools feature 提供，但后端 setup 从不调用
    // open_devtools()；只有用户在这里主动按 F12 / Ctrl+Shift+I 才会打开，
    // 因此不会「一启动应用就自动弹出 DevTools」。
    function toast(msg) {
        var el = document.createElement('div');
        el.textContent = msg;
        el.style.cssText = [
            'position:fixed', 'left:50%', 'bottom:36px', 'transform:translateX(-50%)',
            'max-width:80vw', 'white-space:pre-line', 'background:rgba(28,28,30,.92)',
            'color:#fff', 'padding:10px 18px', 'border-radius:8px', 'font-size:13px',
            'line-height:1.7', 'z-index:2147483647', 'pointer-events:none',
            'box-shadow:0 4px 16px rgba(0,0,0,.3)'
        ].join(';') + ';';
        document.body.appendChild(el);
        setTimeout(function () {
            if (el.parentNode) el.parentNode.removeChild(el);
        }, 2800);
    }

    var SHORTCUT_HELP = [
        'ToyMDict 快捷键',
        'F12 / Ctrl+Shift+I   打开开发者工具（启动时不自动打开）',
        '双击 Ctrl（系统级）   在任意窗口选中文字后取词并搜索',
        'Ctrl+Shift+H         开关「双击 Ctrl 取词」',
        'Ctrl+F               聚焦搜索框',
        'Ctrl+Shift+F         高级搜索',
        'Ctrl+O               导入词典文件',
        'Ctrl+Shift+O         导入词典文件夹',
        'Ctrl+G               词典分组管理',
        'F5 / Ctrl+R          重新载入界面（custom.css 改完可刷新）',
        'Esc                  关闭弹窗 / 图片查看器',
        'Ctrl+/               显示本帮助'
    ].join('\n');

    function safeInvoke(p) {
        if (p && typeof p.catch === 'function') {
            p.catch(function (err) { console.error('快捷键调用失败', err); });
        }
    }

    // 捕获阶段注册，保证优先于页面内其它 keydown 逻辑；只拦截明确列出的组合，
    // 其余按键（含 app.js 的 Esc 关闭弹窗）照常放行。
    document.addEventListener('keydown', function (e) {
        var key = e.key || '';
        var lower = key.toLowerCase();
        var ctrl = e.ctrlKey || e.metaKey;
        var shift = e.shiftKey;
        var alt = e.altKey;
        var api = window.pywebview.api;

        // 1. DevTools：唯一打开入口，启动流程不会走到这里。
        if (key === 'F12' || (ctrl && shift && lower === 'i' && !alt)) {
            e.preventDefault();
            e.stopPropagation();
            safeInvoke(api.open_devtools());
            return;
        }

        // 2. 系统级取词开关 / 快捷键帮助
        if (ctrl && shift && lower === 'h' && !alt) {
            e.preventDefault();
            api.get_global_hotkey_enabled().then(function (on) {
                return api.set_global_hotkey_enabled(!on);
            }).then(function (enabled) {
                toast('双击 Ctrl 取词：' + (enabled ? '已开启' : '已关闭'));
            }).catch(function (err) { console.error('切换取词开关失败', err); });
            return;
        }
        if (ctrl && (key === '/' || key === '?') && !alt) {
            e.preventDefault();
            toast(SHORTCUT_HELP);
            return;
        }

        // 3. 应用内功能键
        if (ctrl && !alt && lower === 'f' && !shift) {
            e.preventDefault();
            var box = document.getElementById('searchInput');
            if (box) { box.focus(); box.select(); }
            return;
        }
        if (ctrl && shift && lower === 'f' && !alt) {
            e.preventDefault();
            if (typeof window.openAdvancedSearch === 'function') window.openAdvancedSearch();
            return;
        }
        if (ctrl && lower === 'o' && !alt) {
            e.preventDefault();
            safeInvoke(shift ? api.open_folder() : api.open_file());
            return;
        }
        if (ctrl && lower === 'g' && !shift && !alt) {
            e.preventDefault();
            if (typeof window.showGroupView === 'function') window.showGroupView();
            return;
        }
        // 浏览器原生刷新键可能被 WebView2 抢先处理；即使重复触发也只是多加载一次前端，
        // 后端状态与配置都在 Rust 侧，重载不会丢数据。
        if (key === 'F5' || (ctrl && lower === 'r' && !shift && !alt)) {
            e.preventDefault();
            window.location.reload();
            return;
        }
    }, true);

    // 后端 evaluate_js 等价通道
    var listening = false;
    function listenJsCall() {
        if (listening) return true;
        var ev = window.__TAURI__ && window.__TAURI__.event;
        if (!ev || !ev.listen) return false;
        listening = true;
        var ready = ev.listen('js_call', function (e) {
            var p = e && e.payload;
            if (!p || !p.fn) return;
            var fn = window[p.fn];
            if (typeof fn === 'function') {
                try {
                    fn.apply(null, p.args || []);
                } catch (err) {
                    console.error('js_call ' + p.fn + ' 执行失败', err);
                }
            }
        });
        // listen 需要一次 IPC 往返才真正注册完成。后端在 get_pending_errors /
        // setup 里推送的启动期 UI（分组下拉框）可能在此之前发出而丢失。
        // 注册完成后派发 pywebviewready，让 app.js 再拉一次：既显示启动期错误，
        // 也让后端重发 updateUI / renderGroupView，保证首屏就有分组数据。
        if (ready && typeof ready.then === 'function') {
            ready.then(function () {
                try {
                    window.dispatchEvent(new Event('pywebviewready'));
                } catch (err) {
                    if (typeof window.pullPendingErrors === 'function') {
                        window.pullPendingErrors();
                    }
                }
            }).catch(function () {});
        }
        return true;
    }
    if (!listenJsCall()) {
        document.addEventListener('DOMContentLoaded', listenJsCall);
        window.addEventListener('load', listenJsCall);
    }

    // custom.css 由用户手动放在 exe 同目录，经自定义协议提供
    invoke('get_protocol_base').then(function (base) {
        if (!base) return;
        // 词条 iframe 的 src 用的就是后端的 protocol_base()，因此它的
        // postMessage origin === base。app.js 顶部同步求值的 ENTRY_ORIGIN
        // 只靠 location.protocol 猜测，这里用后端真实值纠正，避免个别
        // WebView 版本主窗口协议不同导致 isTrustedMessage 误拦截 resize，
        // 使所有词条 iframe 停在 0 高（表现为条目无法展开）。
        window.TOYMDICT_ORIGIN = base;
        window.ENTRY_ORIGIN = base;
        var link = document.createElement('link');
        link.rel = 'stylesheet';
        link.href = base + '/static/custom.css';
        document.head.appendChild(link);
    }).catch(function () { /* 窗口刚创建，稍后由重载生效 */ });

    // ---------------------------------------------------------- 运行期错误弹窗
    // 后端 report_error() 把错误暂存在 pending_errors；app.js 的
    // pullPendingErrors() 只在启动期拉一次，运行期错误会一直堆着不显示。
    // 这里定时拉取并弹窗，让「导入失败 / 词典打不开 / 配置写失败 / 搜索失败」
    // 之类的问题都能被看见。get_pending_errors 会清空队列，因此不会与
    // 启动期拉取重复；后端只在首次拉取时刷新 UI，轮询不会重建分组下拉框。
    function flashErrors(items) {
        (items || []).forEach(function (it) {
            if (typeof window.showError === 'function') {
                window.showError(it[0], it[1], it[2]);
            } else {
                console.error('[ToyMDict] ' + it[0] + ': ' + it[1]);
            }
        });
    }
    setInterval(function () {
        if (!window.pywebview || !window.pywebview.api) return;
        invoke('get_pending_errors').then(flashErrors).catch(function () {});
    }, 1500);

    // ---------------------------------------------------------- 应用内选词
    // 两条互相独立的通道，任一生效即可，避免再次出现「应用内双击 Ctrl 无反应」：
    //   A. 前端本地监听双击 Ctrl：主文档自己读选区；焦点在词条 iframe 内时，
    //      由 iframe 内的回传脚本自行判定并 postMessage 上报（主窗口收不到
    //      iframe 的 keydown）。
    //   B. 后端全局钩子统一模拟 Ctrl+C 读剪贴板（与 Python 版一致），推送
    //      externalSearchFromHotkey；剪贴板没变化时再退回 captureInAppSelection。
    // 用 __lastLocalCapture 时间戳去重，避免同一个词被搜索两次。
    var DOUBLE_CTRL_MS = 400;
    var DEDUP_MS = 800;
    var lastCtrlMs = 0;
    var ctrlDown = false;
    window.__lastLocalCapture = 0;

    function selectedIn(doc, win) {
        try {
            var el = doc && doc.activeElement;
            if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) {
                var s = (el.value || '').slice(el.selectionStart || 0, el.selectionEnd || 0);
                if (s && s.trim()) return s.trim();
            }
            if (win && win.getSelection) {
                var sel = win.getSelection();
                if (sel) {
                    var t = sel.toString();
                    if (t && t.trim()) return t.trim();
                }
            }
        } catch (e) { /* 跨域 iframe 直接跳过 */ }
        return '';
    }

    function finishCapture(raw) {
        if (!raw) return;
        var first = (raw.split(/\r?\n/)[0] || '').trim();
        var text = Array.from(first).slice(0, 200).join('');  // 同外部取词：首行 + 限长
        if (!text) return;
        window.__lastLocalCapture = Date.now();
        if (typeof window.externalSearch === 'function') {
            window.externalSearch(text);
        } else {
            var box = document.getElementById('searchInput');
            if (box) box.value = text;
            if (typeof window.triggerSearch === 'function') window.triggerSearch();
        }
    }

    function allFrames() { return document.querySelectorAll('iframe'); }

    function readLocalSelection() {
        var text = selectedIn(document, window);
        var frames = allFrames();
        for (var i = 0; i < frames.length && !text; i++) {
            text = selectedIn(frames[i].contentDocument, frames[i].contentWindow);
        }
        return text;
    }

    function requestFrameSelection(type) {
        var frames = allFrames();
        for (var j = 0; j < frames.length; j++) {
            try {
                frames[j].contentWindow.postMessage({ type: type }, '*');
            } catch (e) { /* 已卸载的 iframe 忽略 */ }
        }
    }

    // 主文档内双击 Ctrl（搜索框、结果列表、词典信息弹窗等）。
    function onLocalDoubleCtrl() {
        var text = readLocalSelection();
        if (text) { finishCapture(text); return; }
        // 选区在跨域词条 iframe 里时，请 iframe 内脚本把自己读到的选区发回来。
        requestFrameSelection('toymdict-hotkey-selection-request');
    }

    document.addEventListener('keydown', function (e) {
        // 任何非单独 Ctrl 的按键都打断计数：后端模拟的 Ctrl+C 会注入一次
        // Ctrl keydown，不能让它凑成「第三次按下」误触发取词。
        if (e.key !== 'Control' || e.altKey || e.shiftKey || e.repeat) {
            lastCtrlMs = 0;
            ctrlDown = false;
            return;
        }
        if (ctrlDown) return;
        ctrlDown = true;
        var now = Date.now();
        if (lastCtrlMs && now - lastCtrlMs <= DOUBLE_CTRL_MS) {
            lastCtrlMs = 0;
            onLocalDoubleCtrl();
        } else {
            lastCtrlMs = now;
        }
    }, true);
    document.addEventListener('keyup', function (e) {
        if (e.key === 'Control') ctrlDown = false;
    }, true);

    // 词条 iframe 自己判定双击 Ctrl 后主动上报的选区。
    window.addEventListener('message', function (e) {
        var d = e && e.data;
        if (!d || d.type !== 'toymdict-hotkey-selection' || !d.text) return;
        if (Date.now() - window.__lastLocalCapture < DEDUP_MS) return;
        finishCapture(d.text);
    });

    // 后端全局钩子（剪贴板路径）的落点。本地通道刚取过词时直接忽略。
    window.externalSearchFromHotkey = function (text) {
        if (Date.now() - window.__lastLocalCapture < DEDUP_MS) return;
        if (typeof window.externalSearch === 'function') {
            window.externalSearch(text);
        }
    };

    // 后端在「应用内且剪贴板没更新」时的兜底：主文档 / 同源 iframe 读选区，
    // 跨域词条 iframe 用 postMessage 请求回传。
    var inAppPending = false;
    window.captureInAppSelection = function () {
        if (Date.now() - window.__lastLocalCapture < DEDUP_MS) return;
        var text = readLocalSelection();
        if (text) { finishCapture(text); return; }
        var frames = allFrames();
        if (!frames.length || inAppPending) return;

        inAppPending = true;
        var done = false;
        function onMessage(e) {
            var d = e && e.data;
            if (!d || d.type !== 'toymdict-selection' || done || !d.text) return;
            done = true;
            cleanup();
            finishCapture(d.text);
        }
        function cleanup() {
            window.removeEventListener('message', onMessage);
            clearTimeout(timer);
            inAppPending = false;
        }
        var timer = setTimeout(cleanup, 500);
        window.addEventListener('message', onMessage);
        requestFrameSelection('toymdict-get-selection');
    };
})();
