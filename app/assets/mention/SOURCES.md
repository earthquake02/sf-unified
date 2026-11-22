# 单字名扫描词表来源与许可

本目录是裁剪后的运行时子集，供设定提及扫描使用，不包含原始完整词典。

## compounds.txt

来源：fxsjy/jieba 的 `dict.txt`（https://github.com/fxsjy/jieba）

许可：MIT License

裁剪规则：只保留长度 2–4、词频 ≥ 20、且含至少一个汉字的词；忽略词性标记（原词典词性并不可靠）。运行时再与封闭时间词、本书高频邻接词、以及用户例外表合并。

## char-freq.txt

来源：Jun Da《现代汉语单字字频》（Modern Chinese Character Frequency List）

出处：https://lingua.mtsu.edu/chinese-computing/statistics/

本子集只保留「汉字 → 频次位次（rank）」两列，用于单字名的 function / high / mid / rare 分层，不把任一具体汉字写成硬编码特例。

请在再分发时保留上述来源说明。
