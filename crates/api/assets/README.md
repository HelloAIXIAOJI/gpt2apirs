# 后台前端资源

- `htmx.min.js`：htmx 2.0.4，来源 https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js，许可 BSD-2-Clause（见 `htmx.LICENSE`）。
- 模板由服务端 Askama 渲染，页面里没有自己的脚本和样式文件；需要交互时用 htmx 属性，不要新增 JS 文件。

## vendor/glightbox（图片查看器）

- 版本：3.3.0（`glightbox.min.js` / `glightbox.min.css`）
- 许可：MIT，见 `vendor/GLIGHTBOX.LICENSE`
- 用途：图片页「图片浏览」标签的灯箱（上一张/下一张、缩放、键盘、移动端手势）
- 由 `/admin/assets/glightbox.js` 与 `/admin/assets/glightbox.css` 直接内嵌字节输出，不引入打包链
