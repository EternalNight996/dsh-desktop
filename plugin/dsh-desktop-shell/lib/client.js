window.__ModuleLoader__.load({
  id: "dsh-desktop-shell",
  factory: (require) => {
    var module = { exports: {} };
    var exports = module.exports;
    // 桌面壳（client 侧）：DSH 侧边栏底部「更新设置」按钮。
    //
    // 机制与 memory-eternal 的「记忆」按钮完全一致：注册官方槽位
    // sidebar.footer.action（dsh-client-ui-sidebar 渲染在侧边栏底部，
    // props.wide 区分展开/折叠 rail），永不错位、自动跟随 dsh 主题。
    // 点击经 Tauri IPC 打开桌面壳设置窗口；浏览器直开时无 Tauri 环境，静默。
    //
    // i18n：照搬 @eternalnight/dsh-theme 的模式——ctx.locale 注册词典 +
    // bind 拿 t()，槽位 label 带 locale: NS；dsh 里切换中英，按钮实时跟随。
    var React = require('react');

    var NS = 'dsh-desktop-shell';
    var ZH = {
      nav: '更新设置',
      title: '更新设置：检查并更新 dsh 与 dsh-desktop',
      sectionDesc: '检查并更新 dsh 内核与 dsh-desktop 桌面壳。更新由桌面壳统一管理：后台检查版本、一键升级、签名安装包。',
      openSettings: '打开更新设置',
    };
    var EN = {
      nav: 'Update Settings',
      title: 'Update Settings: check and update dsh & dsh-desktop',
      sectionDesc: 'Check and update the dsh engine and the dsh-desktop shell. Updates are managed by the desktop shell: background checks, one-click upgrades, signed installers.',
      openSettings: 'Open update settings',
    };

    var CSS = ''
      + '/* footerActions 直接包含或隔一层 wrapper 都允许换行（:has 在 WebView2/Chromium 均支持） */\n'
      + '[class*="footerActions"]:has(.dsd-footer), :has(> .dsd-footer) { flex-wrap: wrap; width: 100%; }\n'
      + '.dsd-footer { width: 100%; flex: 1 1 100%; }\n'
      + '.dsd-footer-btn { display: flex; align-items: center; gap: 9px; width: 100%; padding: 7px 10px; border: 1px solid var(--dsw-alias-border-l1, rgba(127,127,127,0.14)); background: var(--dsw-alias-bg-layer-1, rgba(255,255,255,0.04)); color: var(--dsw-alias-label-secondary, #6b7280); font: inherit; font-size: 13.5px; line-height: 18px; border-radius: 8px; cursor: pointer; text-align: left; }\n'
      + '.dsd-footer-btn:hover { background: var(--dsw-alias-bg-layer-1, rgba(255,255,255,0.06)); color: var(--dsw-alias-label-primary, #111); }\n'
      + '.dsd-footer-btn:active { transform: translateY(0.5px); }\n'
      + '.dsd-footer-ico { display: inline-flex; flex: none; width: 18px; height: 18px; align-items: center; justify-content: center; }\n'
      + '.dsd-footer-ico svg { width: 18px; height: 18px; }\n'
      + '.dsd-footer-label { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; min-width: 0; }\n'
      + '.dsd-footer.rail .dsd-footer-btn { justify-content: center; padding: 7px 0; }\n'
      + '.dsd-footer.rail .dsd-footer-label { display: none; }\n'
      + '/* 设置页 → 更新设置 面板 */\n'
      + '.dsd-section { padding: 4px 2px; display: flex; flex-direction: column; align-items: flex-start; gap: 0; }\n'
      + '.dsd-section h3 { margin: 0 0 8px; font: inherit; font-size: 16px; font-weight: 600; color: var(--dsw-alias-label-primary, #111); }\n'
      + '.dsd-section p { margin: 0 0 16px; font-size: 13px; line-height: 1.6; opacity: 0.65; }\n'
      + '.dsd-section .dsd-section-btn { width: auto; align-self: flex-start; padding: 8px 16px; color: var(--dsw-alias-label-primary, #111); }\n';

    function RefreshIcon() {
      return React.createElement(
        'svg',
        { width: '18', height: '18', viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor', strokeWidth: '2', strokeLinecap: 'round', strokeLinejoin: 'round', 'aria-hidden': 'true' },
        React.createElement('path', { d: 'M21 2v6h-6' }),
        React.createElement('path', { d: 'M3 12a9 9 0 1 1 15 6.7L21 17' }),
        React.createElement('path', { d: 'M3 22v-6h6' }),
      );
    }

    function openShellSettings() {
      try {
        var tauri = window.__TAURI_INTERNALS__;
        if (tauri && tauri.invoke) tauri.invoke('open_settings');
      } catch (_) {}
    }

    function FooterButton(props) {
      var t = props.t;
      var wide = !(props && props.wide === false);
      return React.createElement(
        'div',
        { className: 'dsd-footer' + (wide ? '' : ' rail') },
        React.createElement('style', null, CSS),
        React.createElement(
          'button',
          { type: 'button', className: 'dsd-footer-btn', onClick: openShellSettings, 'aria-label': t('nav'), title: t('title') },
          React.createElement('span', { className: 'dsd-footer-ico' }, React.createElement(RefreshIcon, null)),
          React.createElement('span', { className: 'dsd-footer-label' }, t('nav')),
        ),
      );
    }

    // 设置页 → 更新设置 面板：说明 + 打开桌面壳设置窗口按钮。
    function SettingsSection(props) {
      var t = props.t;
      return React.createElement(
        'div',
        { className: 'dsd-section' },
        React.createElement('style', null, CSS),
        React.createElement('h3', null, t('nav')),
        React.createElement('p', null, t('sectionDesc')),
        React.createElement('button', { type: 'button', className: 'dsd-footer-btn dsd-section-btn', onClick: openShellSettings }, t('openSettings')),
      );
    }

    var inject = ['settingsScope', 'slots', 'locale'];

    function apply(ctx) {
      // 词典注册 + 绑定（dsh 切语言时槽位经 label/locale: NS 自动重渲染）。
      ctx.effect(() => ctx.locale.register(NS, { zh: ZH, en: EN }), 'dsh-desktop-shell: locale dictionaries');
      var t = ctx.locale.bind(NS);

      ctx.effect(() => ctx.slots.inject('sidebar.footer.action', () => ctx.slots.register(
        { name: 'sidebar.footer.action', id: NS + ':footer', order: 90, label: () => t('nav'), locale: NS, inject: () => ({ t: t }) },
        (props) => React.createElement(FooterButton, props),
      )), 'dsh-desktop-shell: sidebar footer action');

      // 设置页左侧栏「更新设置」section（memory-eternal 的 settings.section 同款机制）。
      ctx.effect(() => ctx.slots.inject('settings.section', () => ctx.slots.register(
        { name: 'settings.section', id: NS, order: 90, label: () => t('nav'), locale: NS, inject: () => ({ t: t }) },
        (props) => React.createElement(SettingsSection, props),
      )), 'dsh-desktop-shell: settings section');
    }

    module.exports = { inject: inject, apply: apply };
    return module.exports;
  },
});
