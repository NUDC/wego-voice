/// <reference types="vite/client" />

// 声明 Vite 注入的模块类型（`*.css`、`*.svg`、`import.meta.env` 等）。
//
// TypeScript 7 起，副作用式 import（`import "./styles.css"`）
// 必须能找到对应的类型声明，否则报 TS2882。
// TS 5 对此是容忍的，所以早先缺了这个文件也没报错。
