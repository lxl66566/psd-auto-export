# PSD 文件自动转换器

一个 Rust 编写的小工具，用于监听指定文件夹（递归）或单个 PSD 文件的创建和保存事件，并自动将其导出为同名的 PNG 文件。

## 安装

在 Release 里下载预编译的二进制文件并放入环境变量。

## 使用方法

```bash
pae /path/to/your/psd/folder                # 监听文件夹
pae /path/to/your/single/file.psd           # 监听单个文件
pae /path/to/your/psd/folder --once         # 导出一次所有 PSD 文件
pae /path/to/your/psd/folder -f jpg         # 导出为 JPG 格式
pae -h                                      # 查看帮助
```

导出的图片文件会保存在 PSD 文件所在的同一目录下，与 PSD 文件同名。
