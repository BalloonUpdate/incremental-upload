use std::cell::Cell;
use std::env;
use std::io::Error;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::Mutex;

use crate::AppResult;
use crate::app_config::AppConfig;
use crate::app_options::AppOptions;
use crate::blocking_thread_pool::BlockingThreadPool;
use crate::file::File;
use crate::file_comparer::FileComparer;
use crate::file_state::State;
use crate::hash_cache::HashCache;
use crate::rule_filter::RuleFilter;
use crate::simple_file::FileData;
use crate::subprocess_task::SubprocessResult;
use crate::subprocess_task::SubprocessTask;
use crate::variable_replace::VariableReplace;

pub struct App {
    options: AppOptions,
    config: AppConfig,
    variables: VariableReplace,
    hash_cache: Arc<HashCache>,
    file_filter: RuleFilter,
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

        let hash_cache = Arc::new(HashCache::new(&sourcedir));
        let file_filter = RuleFilter::new(&config.file_filters)?;

        let mut variables = VariableReplace::new();
        variables.variables.extend(config.variables.to_owned());

        variables.add("source", &sourcedir.path());
        variables.add("workdir", &workdir.path());
        variables.add("source_", &sourcedir.path().replace("\\", "/"));
        variables.add("workdir_", &workdir.path().replace("\\", "/"));
        
        Ok(App {
            options,
            config,
            variables,
            hash_cache,
            file_filter,
            sourcedir,
            workdir,
        })
    }

    fn execute_multiple_thread(
        &self, 
        commands: &Vec<Vec<String>>, 
        parallel: usize, 
        varses: &Vec<VariableReplace>,
        before_execute: Box<dyn Fn(&VariableReplace) + Send + Sync>,
        after_execute: Box<dyn Fn(&VariableReplace) + Send + Sync>
    ) -> AppResult<()> {
        let mut pool = BlockingThreadPool::new(parallel);
        let after_execute = Arc::new(after_execute);

        for vars in varses {
            let vars = vars.clone();
            let workdir = self.workdir.clone();
            let debug = self.options.debug;
            let commands = commands.clone();
            let after_execute = after_execute.clone();

            before_execute(&vars);
            
            pool.execute(move || {
                let mut last_result: Option<SubprocessResult> = None;
                for step in commands {
                    let mut task = SubprocessTask::from_command_line(
                        &step, &workdir, &vars, 
                        last_result.as_ref()).unwrap();
        
                    if debug {
                        println!("> {:?}", task.raw_divided);
                    }
        
                    let r = task.execute(false);
                    if r.is_err() {
                        return Err(Box::new(r.err().unwrap()));
                    }
                    let r = r.unwrap();
                    last_result = Some(r);
                }

                after_execute(&vars);

                Ok(())
            });

            let r = pool.close_and_wait();

            if r.is_err() {
                let err = r.err().unwrap();
                return Err(err);
            }
        }

        Ok(())
    }

    fn execute_single_thread(&self, commands: &Vec<Vec<String>>, vars: &VariableReplace) -> AppResult<()> {
        let mut last_result: Option<SubprocessResult> = None;
        for step in commands {
            let mut task = SubprocessTask::from_command_line(
                step, &self.workdir, vars, 
                last_result.as_ref())?;

            if self.options.debug {
                println!("> {:?}", task.raw_divided);
            }

            last_result = Some(task.execute(false)?);
        }

        Ok(())
    }

    fn get_state_file(&self) -> File {
        File::new(&self.variables.apply(&self.config.state_file))
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
                    self.execute_single_thread(&self.config.download_state, &self.variables)?;
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

    pub fn save_state_file(&self, comparer: &FileComparer, state_file: &File, state: &State) -> AppResult<()> {
        let update_local_state = self.config.use_local_state;
        let update_remote_state = self.config.use_remote_state;

        if comparer.differences.has_differences() && (update_local_state || update_remote_state) {
            if update_local_state {
                println!("更新本地状态文件...");
            }
            
            if state_file.exists() {
                state_file.rm()?;
            }
            
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

                if !self.config.upload_state.is_empty() {
                    self.execute_single_thread(&self.config.upload_state, &self.variables)?;
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
        
        // 计算差异
        let mut comparer = FileComparer::new(&self.sourcedir, Box::new(compare_func), &self.hash_cache, self.config.fast_comparison, &self.file_filter, self.options.debug);
        println!("正在计算文件差异...");
        comparer.compare(&self.sourcedir, &state)?;

        Ok(comparer)
    }

    pub fn execute_operations(&self, comparer: &FileComparer, state: Arc<Mutex<Cell<State>>>) -> AppResult<()> {
        let diff = &comparer.differences;

        println!(
            "旧文件: {}, 旧目录: {}, 新文件: {}, 新目录: {}", 
            diff.old_files.len(), diff.old_folders.len(),
            diff.new_files.len(), diff.new_folders.len(),
        );

        // 执行用户初始化指令
        if comparer.differences.has_differences() && !self.config.start_up.is_empty() {
            self.execute_single_thread(&self.config.start_up, &self.variables)?;
        }
        
        // 删除文件
        {
            let filtered_old_files = diff.old_files
                .iter()
                .filter_map(|e| if self.config.overlay_mode && diff.new_files.contains(e) { None } else { Some(&e[..]) })
                .collect::<Vec<&str>>();
            let total = filtered_old_files.len();
            let done = Arc::new(Mutex::new(0));

            if !self.config.delete_file.is_empty() {
                let varses = filtered_old_files.iter().map(|f| {
                    let mut vars = self.variables.to_owned();
                    vars.add("path", f);
                    vars.add("path_", &f.replace("/", "\\"));
                    vars
                }).collect::<Vec<VariableReplace>>();

                let state = state.clone();

                self.execute_multiple_thread(
                    &self.config.delete_file, 
                    self.config.threads as usize, 
                    &varses, 
                    Box::new(move |vars| {
                        let mut done = done.lock().unwrap();
                        *done += 1;
                        println!("删除文件({}/{}): {}", done, total, vars.variables.get("path").unwrap());
                    }),
                    Box::new(move |vars| {
                        let path = vars.variables.get("path").unwrap();
                        state.clone().lock().unwrap().get_mut().remove_file_or_dir(&path);
                    })
                )?;
            } else {
                for f in &filtered_old_files {
                    let mut done = done.lock().unwrap();
                    *done += 1;
                    state.lock().unwrap().get_mut().remove_file_or_dir(&f);
                    println!("删除文件({}/{}): {}", done, total, f);
                }
            }

            // 同步更新状态(删除剩余的文件)
            for d in &diff.old_files {
                if !filtered_old_files.contains(&(&d[..])) {
                    state.lock().unwrap().get_mut().remove_file_or_dir(d);
                }
            }
        }

        // 删除目录
        {
            let total = &diff.old_folders.len();
            let mut done = 0;
            for f in &diff.old_folders {
                let mut vars = self.variables.to_owned();
                vars.add("path", f);
                vars.add("path_", &f.replace("/", "\\"));

                done += 1;
                println!("删除目录({}/{}): {}", done, total, f);

                if !self.config.delete_dir.is_empty() {
                    self.execute_single_thread(&self.config.delete_dir, &vars)?;
                }

                state.lock().unwrap().get_mut().remove_file_or_dir(f);
            }
        }

        // 创建目录
        {
            let total = &diff.new_folders.len();
            let mut done = 0;
            for f in &diff.new_folders {
                let mut vars = self.variables.to_owned();
                vars.add("path", f);
                vars.add("path_", &f.replace("/", "\\"));

                done += 1;
                println!("新目录({}/{}): {}", done, total, f);

                if !self.config.upload_dir.is_empty() {
                    self.execute_single_thread(&self.config.upload_dir, &vars)?;
                }

                state.lock().unwrap().get_mut().make_dir(f);
            }
        }

        // 上传文件
        {
            let total = diff.new_files.len();
            let done = Arc::new(Mutex::new(0));
    
            if !self.config.upload_file.is_empty() {
                let varses = diff.new_files.iter().map(|f| {
                    let mut vars = self.variables.to_owned();
                    vars.add("path", f);
                    vars.add("path_", &f.replace("/", "\\"));
                    vars
                }).collect::<Vec<VariableReplace>>();
    
                let sourcedir = self.sourcedir.to_owned();
                let hash_cache = self.hash_cache.clone();
                let debug = self.options.debug;
                let state = state.clone();
    
                self.execute_multiple_thread(
                    &self.config.upload_file, 
                    self.config.threads as usize, 
                    &varses, 
                    Box::new(move |vars| {
                        let mut done = done.lock().unwrap();
                        *done += 1;
                        println!("新文件({}/{}): {}", done, total, vars.variables.get("path").unwrap());
                    }),
                    Box::new(move |vars| {
                        let path = vars.variables.get("path").unwrap();
                        state.lock().unwrap().get_mut().add_file(&path, &sourcedir, &hash_cache, debug);
                    })
                )?;
            } else {
                for f in &diff.new_files {
                    let mut done = done.lock().unwrap();
                    *done += 1;
                    println!("新文件({}/{}): {}", done, total, f);
                    state.lock().unwrap().get_mut().add_file(f, &self.sourcedir, &self.hash_cache, self.options.debug);
                }
            }
        }

        // 执行用户清理指令
        if comparer.differences.has_differences() && !self.config.clean_up.is_empty() {
            self.execute_single_thread(&self.config.clean_up, &self.variables)?;
        }

        println!(
            "旧文件: {}, 旧目录: {}, 新文件: {}, 新目录: {}", 
            diff.old_files.len(), diff.old_folders.len(),
            diff.new_files.len(), diff.new_folders.len(),
        );

        Ok(())
    }

    fn test_filter(&self) -> AppResult<()> {
        fn walk(directory: &File, base: &File, filter: &RuleFilter) -> AppResult<()> {
            for f in directory.files()? {
                let f = f?;
                let relative_path = f.relativized_by(base);
                let matched = filter.test_all(&relative_path, true);
                if matched {
                    println!("matched: {}", relative_path);
                }

                if f.is_dir() {
                    walk(&f, base, filter)?;
                }
            }

            Ok(())
        }

        walk(&self.sourcedir, &self.sourcedir, &self.file_filter)?;

        Ok(())
    }

    pub fn main(&mut self) -> AppResult<()> {
        if self.options.test_filter {
            self.test_filter()?;
            return Ok(());
        }

        let state_file = self.get_state_file();
        let state = Arc::new(Mutex::new(Cell::new(self.load_state_from_file(&state_file)?)));
        let comparer = self.compare_files(state.lock().unwrap().get_mut())?;

        // 执行远端读写操作
        let result = self.execute_operations(&comparer, state.clone());
        
        if result.is_err() {
            println!("更新状态时出现错误，保存状态文件");
        }

        // 更新状态文件
        self.save_state_file(&comparer, &state_file, state.lock().unwrap().get_mut())?;

        result?;

        Ok(())
    }
}