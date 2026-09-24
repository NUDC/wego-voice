wego-neural —— 神经声线转换的检查工具

  wego-neural inspect <模型.onnx>            打印模型的输入/输出契约
  wego-neural encode  <模型.onnx> <干声.wav>  跑一遍内容编码器并自检

⚠️ 这个 crate 默认不编译（ort 是可选特性）。
   开发时用：cargo run -p voice-neural --features onnx --bin wego-neural -- ...
