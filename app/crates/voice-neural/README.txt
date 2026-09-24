wego-neural —— 神经声线转换的检查工具

  wego-neural inspect <模型.onnx>            打印模型的输入/输出契约
  wego-neural encode   <模型.onnx> <干声.wav>  跑一遍内容编码器并自检
  wego-neural features <模型.onnx> <干声.wav>  整条特征管线（f0+音量+内容，对齐）
  wego-neural keys     <检查点.pt>             列出检查点里的参数名与形状
  wego-neural check    <检查点.pt>             拿检查点跟本实现的期望对一遍
  wego-neural convert  <编码器.onnx> <解码器.pt> <干声.wav> <输出.wav>

⚠️ 这个 crate 默认不编译（ort 是可选特性）。
   开发时用：cargo run -p voice-neural --features onnx --bin wego-neural -- ...
