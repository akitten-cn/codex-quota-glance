/*
 * 任务栏本体与设置实时预览共享的视觉契约。
 *
 * 此文件只描述已经确认的“本体样式”，不保存用户设置或真实账户数据。
 * 用户可调的宽度、停靠位置、避让距离和毛玻璃强度由设置页通过
 * postMessage 注入；这样预览不会再维护一份容易漂移的近似样式。
 */
window.CodexTaskbarVisualContract = Object.freeze({
  version: '10',
  previewScenario: 'normal',
  previewActivity: 'executing',
  // 未使用区域只显示 CSS 毛玻璃层，不让 WebGL 叠加深色底或流光。
  unusedWebglAlpha: 0,
  unusedAnimation: false,
  outerBorder: 'none',
  frost: Object.freeze({ min: 0.13, max: 0.58, default: 0.294 })
});
