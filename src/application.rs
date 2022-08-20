use std::env;
use std::io::Error;
use std::io::ErrorKind;
use std::process::Command;
use regex::Regex;

use crate::AppResult;
use crate::app_config::AppConfig;
use crate::app_options::AppOptions;
use crate::blocking_thread_pool::BlockingThreadPool;
use crate::file::File;
use crate::file_comparer::FileComparer;
use crate::file_state::State;
use crate::hash_cache::HashCache;
use crate::simple_file::FileData;
use crate::subprocess_task::SubprocessTask;
use crate::utils::command_split;
use crate::variable_replace::VariableReplace;

pub struct App {
    options: AppOptions,
    config: AppConfig,
    variables: VariableReplace,
    hash_cache: HashCache,
    sourcedir: File,
    workdir: File,
}

impl App {
    pub fn new() -> AppResult<App> {
        let options = AppOptions::parse_from_command_line();

        // 检查参数
        let config_file = File::new(&options.config);
        if !config_file.is_file() {
            return Err(Box::new(Error::new(ErrorKind::NotFound, String::from(format!("the config file is not a file: {}", options.config)))))
        }

        let config = AppConfig::parse_from_yaml_string(config_file.read()?)?;
        
        // 检查参数
        let source_dir = &config.source_dir;
        let source_dir = if source_dir.ends_with("/") { &source_dir[0..source_dir.len() - 1] } else { &source_dir[..] }.to_owned();
        let sourcedir = File::new(&source_dir);
        if !sourcedir.is_dir() {
            return Err(Box::new(Error::new(ErrorKind::NotFound, String::from(format!("the source-directory is not a dir: {}", config.source_dir)))))
        }

        let workdir = &config.command_workdir;
        let workdir = if workdir.len() > 0 { File::new(&workdir) } else { File::from(env::current_dir().expect("failed to get Current Work Directory."))};
        if !workdir.is_dir() {
            return Err(Box::new(Error::new(ErrorKind::NotFound, String::from(format!("the workdir is not a dir: {}", workdir.path())))))
        }

        let hash_cache = HashCache::new(&sourcedir);

        let mut variables = VariableReplace::new();
        variables.variables.extend(config.variables.to_owned());

        variables.add("source", sourcedir.path());
        variables.add("workdir", workdir.path());
        
        Ok(App {
            options,
            config,
            variables,
            hash_cache,
            sourcedir,
            workdir,
        })
    }

    fn build_subprocesses(&self, commands: &Vec<Vec<String>>, vars: &VariableReplace) -> AppResult<Vec<SubprocessTask>> {
        let mut result: Vec<SubprocessTask> = Vec::new();

        for command in commands {
            result.push(self.build_subprocess(command, vars)?);
        }

        Ok(result)
    }

    fn build_subprocess(&self, command_devided: &Vec<String>, vars: &VariableReplace) -> AppResult<SubprocessTask> {
        let workdir: &File = &self.workdir;
        let debug: bool = self.options.debug;
        let dry_run: bool = self.options.dryrun;

        if command_devided.is_empty() {
            return Err(Box::new(Error::new(ErrorKind::InvalidInput, "subprocess command line must be not empty")));
        }

        let mut command_devided = command_devided.iter().map(|s| vars.apply(s)).collect::<Vec<String>>();

        let do_not_split = command_devided[0].starts_with("+");
        if do_not_split {
            command_devided[0] = (&(command_devided[0])[1..]).to_owned();
        }

        if !do_not_split && command_devided.len() == 1 {
            command_devided = command_split(&command_devided[0]);
        }

        let prog = command_devided.first().unwrap().clone(); 
        let args = if command_devided.len() > 0 { command_devided[1..].to_vec() } else { vec![] };
        
        let workdir = vars.apply(workdir.path());
        let mut subprocess = Command::new(prog.to_owned());

        let path_separator = if cfg!(target_os = "windows") { ";" } else { ":" };
        let path = subprocess.get_envs().filter_map(|(k, v)| if k == "PATH" { 
            v.map_or_else(|| None, |value| Some(value.to_str().unwrap().to_owned()))
        } else { None }).next();
        subprocess.env("PATH", &((if path.is_some() { path.unwrap() + path_separator } else { "".to_string() }) + &workdir));
        subprocess.args(args.to_owned());
        subprocess.current_dir(workdir.to_owned());

        Ok(SubprocessTask::new(if !dry_run { Some(subprocess) } else { None }, command_devided, debug, false))
    }

    fn get_state_file(&self) -> File {
        File::new(&self.variables.apply(&self.config.state_file)[..])
    }

    pub fn load_state_from_file(&self, state_file: &File) -> AppResult<State> {
        let use_local_state = self.config.use_local_state;
        let use_remote_state = self.config.use_remote_state;

        let state = if use_local_state || use_remote_state {
            if use_local_state {
                println!("从本地加载状态文件")
            } else if use_remote_state {
                println!("从远端更新状态文件");
                if !self.config.download_state.is_empty() {
                    for mut p in self.build_subprocesses(&self.config.download_state, &self.variables)? {
                        p.execute()?;
                    }
                }
            }

            if !state_file.exists() {
                println!("未找到任何状态文件!使用默认的空状态!");
                json::JsonValue::new_array()
            } else {
                json::parse(&state_file.read().unwrap()[..])
                .expect(&format!("状态文件无法解析为Json格式: {}", state_file.path())[..])
            }
        } else {
            println!("不加载任何状态文件!使用默认的空状态!");
            json::JsonValue::new_array()
        };
        
        Ok(State::from_json_array(&state))
    }

    pub fn save_state_file(&self, comparer: &FileComparer, state_file: &File, state: &mut State) -> AppResult<()> {
        // let state_file = File::new("state-out.json");

        let update_local_state = self.config.use_local_state;
        let update_remote_state = self.config.use_remote_state;

        if comparer.differences.has_differences() && (update_local_state || update_remote_state) {
            if update_local_state {
                println!("更新本地状态文件...");
            }
            
            if state_file.exists() {
                state_file.rm()?;
            }
            
            state.update_from_differences(&comparer.differences, &self.sourcedir, &self.hash_cache, self.options.debug);
            let file_contents = state.to_json_array();
            let file_contents = if self.config.state_indent > 0 { 
                file_contents.pretty(self.config.state_indent as u16)
            } else { 
                file_contents.dump() 
            };

            state_file.parent()?.unwrap().mkdirs()?;
            state_file.write(&file_contents)?;

            // 更新远端状态文件
            if update_remote_state {
                println!("更新远端状态文件...");

                let mut vars = self.variables.to_owned();
                vars.add("apath", state_file.path());

                if !self.config.upload_state.is_empty() {
                    for mut p in self.build_subprocesses(&self.config.upload_state, &vars)? {
                        p.execute()?;
                    }
                }
            }

            // 不保留本地状态文件
            if !update_local_state {
                if state_file.exists() {
                    state_file.rm()?;
                }
            }
        }

        Ok(())
    }

    pub fn compare_files(&self, state: &State) -> AppResult<FileComparer> {
        let compare_func = |remote: &FileData, local: &File, path: &str, fast_comparison: bool, hash_cache: &HashCache, debug_mode: bool| -> bool {
            (fast_comparison && remote.modified == local.modified().map_or_else(|_e| 0, |v| v)) || 
            remote.sha1 == hash_cache.get_hash(path, debug_mode)
        };
        
        // 预编译正则表达式
        let mut regexes_compiled = Vec::<Regex>::new();
        for pattern in &self.config.file_filters {
            let pat = Regex::new(&pattern[..]);
            if pat.is_err() {
                return Err(Box::new(Error::new(ErrorKind::InvalidInput, "fail to compile the regex: ".to_string() + &pattern)));
            }
            regexes_compiled.push(pat.unwrap());
        }
        
        // 计算差异
        let mut comparer = FileComparer::new(&self.sourcedir, Box::new(compare_func), &self.hash_cache, self.config.fast_comparison, regexes_compiled, self.options.debug);
        println!("正在计算文件差异...");
        comparer.compare(&self.sourcedir, &state)?;

        Ok(comparer)
    }

    pub fn execute_operations(&self, comparer: &FileComparer) -> AppResult<()> {
        let diff = &comparer.differences;

        println!(
            "旧文件: {}, 旧目录: {}, 新文件: {}, 新目录: {}", 
            diff.old_files.len(), diff.old_folders.len(),
            diff.new_files.len(), diff.new_folders.len(),
        );

        // 执行用户初始化指令
        if comparer.differences.has_differences() && !self.config.start_up.is_empty() {
            for mut p in self.build_subprocesses(&self.config.start_up, &self.variables)? {
                p.execute()?;
            }
        }
        
        // 删除文件
        let pool = BlockingThreadPool::new(self.config.threads as usize);
        let filtered_old_files = diff.old_files
            .iter()
            .filter_map(|e| if self.config.overlay_mode && diff.new_files.contains(e) { None } else { Some(&e[..]) })
            .collect::<Vec<&str>>();
        let total = filtered_old_files.len();
        let mut done = 0;
        for f in filtered_old_files {
            let mut vars = self.variables.to_owned();
            vars.add("path", f);

            done += 1;
            println!("删除文件({}/{}): {}", done, total, f);

            if !self.config.delete_file.is_empty() {
                let sp = self.build_subprocesses(&self.config.delete_file, &vars)?;
                pool.execute(move || for mut p in sp { p.execute().unwrap() })
            }
        }
        drop(pool);

        // 删除目录
        let pool = BlockingThreadPool::new(self.config.threads as usize);
        let total = &diff.old_folders.len();
        let mut done = 0;
        for f in &diff.old_folders {
            let mut vars = self.variables.to_owned();
            vars.add("path", f);

            done += 1;
            println!("删除目录({}/{}): {}", done, total, f);

            if !self.config.delete_dir.is_empty() {
                let sp = self.build_subprocesses(&self.config.delete_dir, &vars)?;
                pool.execute(move || for mut p in sp { p.execute().unwrap() })
            }
        }
        drop(pool);

        // 创建目录
        let pool = BlockingThreadPool::new(self.config.threads as usize);
        let total = &diff.new_folders.len();
        let mut done = 0;
        for f in &diff.new_folders {
            let mut vars = self.variables.to_owned();
            vars.add("path", f);

            done += 1;
            println!("新目录({}/{}): {}", done, total, f);

            if !self.config.upload_dir.is_empty() {
                let sp = self.build_subprocesses(&self.config.upload_dir, &vars)?;
                pool.execute(move || for mut p in sp { p.execute().unwrap() })
            }
        }
        drop(pool);

        // 上传文件
        let pool = BlockingThreadPool::new(self.config.threads as usize);
        let total = &diff.new_files.len();
        let mut done = 0;
        for f in &diff.new_files {
            let mut vars = self.variables.to_owned();
            vars.add("path", f);

            done += 1;
            println!("新文件({}/{}): {}", done, total, f);

            if !self.config.upload_file.is_empty() {
                let sp = self.build_subprocesses(&self.config.upload_file, &vars)?;
                pool.execute(move || { for mut p in sp { p.execute().unwrap(); }})
            }
        }
        drop(pool);

        // 执行用户清理指令
        if comparer.differences.has_differences() && !self.config.clean_up.is_empty() {
            for mut p in self.build_subprocesses(&self.config.clean_up, &self.variables)? {
                p.execute()?;
            }
        }

        Ok(())
    }

    pub fn main(&mut self) -> AppResult<()> {
        let state_file = self.get_state_file();
        let mut state = self.load_state_from_file(&state_file)?;
        let comparer = self.compare_files(&state)?;

        // 执行远端读写操作
        self.execute_operations(&comparer)?;
        
        // 更新状态文件
        self.save_state_file(&comparer, &state_file, &mut state)?;

        Ok(())
    }
}