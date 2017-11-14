use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::Arc;
use std::usize;
use rocket::response::NamedFile;
use std::fs::Metadata;
use std::fs;

use cached_file::CachedFile;
use responder_file::ResponderFile;

use in_memory_file::InMemoryFile;
use priority_function::{PriorityFunction, default_priority_function};





#[derive(Debug, PartialEq)]
enum CacheInvalidationError {
    NoMoreFilesToRemove,
    NewPriorityIsNotHighEnough,
    NewFileSmallerThanMin,
    NewFileLargerThanMax,
    NewFileLargerThanCache,
    InvalidMetadata,
    InvalidPath,
}

#[derive(Debug, PartialEq)]
enum CacheInvalidationSuccess {
    ReplacedFile,
    InsertedFileIntoAvailableSpace,
}

#[derive(Debug, PartialEq, Clone)]
pub struct AccessCountAndPriority {
    access_count: usize,
    priority_score: usize
}

#[derive(Debug, PartialEq, Clone)]
pub struct FileStats {
    size: usize,
    access_count: usize,
    priority: usize
}

/// The cache holds a number of files whose bytes fit into its size_limit.
/// The cache acts as a proxy to the filesystem, returning cached files if they are in the cache,
/// or reading a file directly from the filesystem if the file is not in the cache.
///
/// When the cache is full, each file in the cache will have priority score determined by a provided
/// priority function.
/// When a a new file is attempted to be stored, it will calculate the priority of the new score and
/// compare that against the score of the file with the lowest priority in the cache.
/// If the new file's priority is higher, then the file in the cache will be removed and replaced with the new file.
/// If removing the first file doesn't free up enough space for the new file, then the file with the
/// next lowest priority will have its priority added to the other removed file's and the aggregate
/// cached file's priority will be tested against the new file's.
///
/// This will repeat until either enough space can be freed for the new file, and the new file is
/// inserted, or until the priority of the cached files is greater than that of the new file,
/// in which case, the new file isn't inserted.
#[derive(Debug)]
pub struct Cache {
    pub size_limit: usize, // The number of bytes the file_map should ever hold.
    pub(crate) min_file_size: usize, // The minimum size file that can be added to the cache
    pub(crate) max_file_size: usize, // The maximum size file that can be added to the cache
    pub(crate) priority_function: PriorityFunction, // The priority function that is used to determine which files should be in the cache.
    pub(crate) file_map: HashMap<PathBuf, Arc<InMemoryFile>>, // Holds the files that the cache is caching
    pub(crate) file_stats_map: HashMap<PathBuf, FileStats>, // Holds stats for only the files in the file map.
    pub(crate) access_count_map: HashMap<PathBuf, usize>, // Every file that is accessed will have the number of times it is accessed logged in this map.
}


impl Cache {
    /// Creates a new Cache with the given size limit and the default priority function.
    /// The min and max file sizes are not set.
    ///
    /// # Arguments
    ///
    /// * `size_limit` - The number of bytes that the Cache is allowed to hold at a given time.
    ///
    /// # Example
    ///
    /// ```
    /// use rocket_file_cache::Cache;
    /// let mut cache = Cache::new(1024 * 1024 * 30); // Create a cache that can hold 30 MB of files
    /// ```
    pub fn new(size_limit: usize) -> Cache {
        Cache {
            size_limit,
            min_file_size: 0,
            max_file_size: usize::MAX,
            priority_function: default_priority_function,
            file_map: HashMap::new(),
            file_stats_map: HashMap::new(),
            access_count_map: HashMap::new(),
        }
    }

    /// Either gets the file from the cache if it exists there, gets it from the filesystem and
    /// tries to cache it, or fails to find the file and returns None.
    ///
    /// # Arguments
    ///
    /// * `pathbuf` - A pathbuf that represents the path of the file in the filesystem. The pathbuf
    /// also acts as a key for the file in the cache.
    /// The path will be used to find a cached file in the cache or find a file in the filesystem if
    /// an entry in the cache doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// #![feature(attr_literals)]
    /// #![feature(custom_attribute)]
    /// # extern crate rocket;
    /// # extern crate rocket_file_cache;
    ///
    /// # fn main() {
    /// use rocket_file_cache::{Cache, ResponderFile};
    /// use std::sync::Mutex;
    /// use std::path::{Path, PathBuf};
    /// use rocket::State;
    /// use rocket::response::NamedFile;
    ///
    ///
    /// #[get("/<file..>")]
    /// fn files(file: PathBuf, cache: State<Mutex<Cache>> ) -> Option<ResponderFile> {
    ///     let pathbuf: PathBuf = Path::new("www/").join(file).to_owned();
    ///
    ///     // Try to lock the cache in order to use it.
    ///     match cache.try_lock() {
    ///         Ok(mut cache) => cache.get(&pathbuf),
    ///         Err(_) => {
    ///             // Fall back to using the FS if another thread owns the lock.
    ///             match NamedFile::open(pathbuf).ok() {
    ///                 Some(file) => Some(ResponderFile::from(file)),
    ///                 None => None
    ///             }
    ///         }
    ///     }
    /// }
    /// # }
    /// ```
    pub fn get(&mut self, pathbuf: &PathBuf) -> Option<ResponderFile> {
        trace!("{:#?}", self);
        // First, try to get the file in the cache that corresponds to the desired path.
        {
            if let Some(cache_file) = self.get_from_cache(pathbuf) {
                debug!("Cache hit for file: {:?}", pathbuf);
                self.increment_access_count(pathbuf); // File is in the cache, increment the count
                self.update_stats(pathbuf);
                return Some(ResponderFile::from(cache_file));
            }
        }
        // TODO Consider if I should have a check step that just checks if the file will pass the tests, then another step that just inserts it.
        // If the file can't be immediately accessed, try to get it from the FS.
        self.try_insert(pathbuf.clone()).ok()
    }




    /// If a file has changed on disk, the cache will not automatically know that a change has occurred.
    /// Calling this function will check if the file exists, read the new file into memory,
    /// replace the old file, and update the priority score to reflect the new size of the file.
    ///
    ///  # Arguments
    ///
    /// * `pathbuf` - A pathbuf that represents the path of the file in the filesystem, and key in the cache.
    /// The path will be used to find the new file in the filesystem and find the old file to replace in
    /// the cache.
    pub fn refresh(&mut self, pathbuf: &PathBuf) -> bool {

        let mut is_ok_to_refresh: bool = false;

        // Check if the file exists in the cache
        if self.file_map.contains_key(pathbuf)  {
            // See if the new file exists.
            let path_string: String = match pathbuf.clone().to_str() {
                Some(s) => String::from(s),
                None => return false
            };
            if let Ok(metadata) = fs::metadata(path_string.as_str()) {
                if metadata.is_file() {
                    // If the stats for the old file exist
                    if self.file_stats_map.contains_key(pathbuf) {
                        is_ok_to_refresh = true;
                    }
                }
            };
        }

        if is_ok_to_refresh {
            if let Ok(new_file) = InMemoryFile::open(pathbuf.clone()) {
                debug!("Refreshing file: {:?}", pathbuf);
                {
                    self.file_map.remove(pathbuf);
                    self.file_map.insert(pathbuf.clone(), Arc::new(new_file));
                }

                self.update_stats(pathbuf)

            }
        }
        is_ok_to_refresh
    }

    // TODO, add checks and return an enum indicating what happened.
    /// Removes the file from the cache.
    /// This will not reset the access count, so the next time the file is accessed, it will be added to the cache again.
    /// The access count will have to be reset separately.
    ///
    /// # Arguments
    ///
    /// * `pathbuf` - A pathbuf that acts as a key to the file that should be removed from the cache
    ///
    /// # Example
    ///
    /// ```
    /// use rocket_file_cache::Cache;
    /// use std::path::PathBuf;
    ///
    /// let mut cache = Cache::new(1024 * 1024 * 10);
    /// let pathbuf = PathBuf::new();
    /// cache.remove(&pathbuf);
    /// assert!(cache.contains_key(&pathbuf) == false);
    /// ```
    pub fn remove(&mut self, pathbuf: &PathBuf) {
        self.file_stats_map.remove(pathbuf);
        self.file_map.remove(pathbuf);
        let entry = self.access_count_map.entry(pathbuf.clone()).or_insert(
            0
        );
        *entry = 0
    }

    /// Returns a boolean indicating if the cache has an entry corresponding to the given key.
    ///
    /// # Arguments
    ///
    /// * `pathbuf` - A pathbuf that is used as a key to look up the file.
    ///
    /// # Example
    ///
    /// ```
    /// use rocket_file_cache::Cache;
    /// use std::path::PathBuf;
    ///
    /// let mut cache = Cache::new(1024 * 1024 * 20);
    /// let pathbuf: PathBuf = PathBuf::new();
    /// cache.get(&pathbuf);
    /// assert!(cache.contains_key(&pathbuf) == false);
    /// ```
    pub fn contains_key(&self, pathbuf: &PathBuf) -> bool {
        self.file_map.contains_key(pathbuf)
    }


    /// Gets the sum of the sizes of the files that are stored in the cache.
    ///
    /// # Example
    ///
    /// ```
    /// use rocket_file_cache::Cache;
    ///
    /// let cache = Cache::new(1024 * 1024 * 30);
    /// assert!(cache.used_bytes() == 0);
    /// ```
    pub fn used_bytes(&self) -> usize {
        self.file_map.iter().fold(0usize, |size, x| size + x.1.size)
    }

    /// Gets the size of the file from the file's metadata.
    /// This avoids having to read the file into memory in order to get the file size.
    fn get_file_size_from_metadata(path: &PathBuf) -> Result<usize, CacheInvalidationError> {
        let path_string: String = match path.clone().to_str() {
            Some(s) => String::from(s),
            None => return Err(CacheInvalidationError::InvalidPath)
        };
        let metadata: Metadata = match fs::metadata(path_string.as_str()) {
            Ok(m) => m,
            Err(_) => return Err(CacheInvalidationError::InvalidMetadata)
        };
        let size: usize = metadata.len() as usize;
        Ok(size)
    }


    /// Attempt to store a given file in the the cache.
    /// Storing will fail if the current files have more access attempts than the file being added.
    /// If the provided file has more more access attempts than one of the files in the cache,
    /// but the cache is full, a file will have to be removed from the cache to make room
    /// for the new file.
    ///
    /// If the insertion works, the cache will update the priority score for the file being inserted.
    /// The cached priority score requires the file in question to exist in the file map, so it will
    /// have a size to use when calculating.
    ///
    /// It will get the size of the file to be inserted.
    /// If will use this size to check if the file could be inserted.
    /// If it can be inserted, it reads the file into memory, stores a copy of the in-memory file behind a pointer, and constructs
    /// a RespondableFile to return.
    ///
    /// If the file can't be added, it will open a NamedFile and construct a RespondableFile from that,
    /// and return it.
    /// This means that it doesn't need to read the whole file into memory before reading through it
    /// again to set the response body.
    /// The lack of the need to read the whole file twice keeps performance of cache misses on par
    /// with just normally reading the file without a cache.
    ///
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the file to be stored. Acts as a key for the file in the cache. Is used
    /// look up the location of the file in the filesystem if the file is not in the cache.
    ///
    ///
    fn try_insert(&mut self, path: PathBuf) -> Result< ResponderFile, CacheInvalidationError> {

        let size = Cache::get_file_size_from_metadata(&path)?;
        // If the FS can read metadata for a file, then the file exists, and it should be safe to increment
        // the access_count and update.

        // Since these are updated here, the
        self.increment_access_count(&path);
        self.update_stats(&path);

        // Determine how much space can still be used (represented by a negative value) or how much
        // space needs to be freed in order to make room for the new file
        let required_space_for_new_file: isize = (self.used_bytes() as isize + size as isize) - self.size_limit as isize;


        if size > self.max_file_size || size < self.min_file_size {

            debug!("File does not fit size constraints of the cache.");
            match NamedFile::open(path) {
                Ok(named_file) => return Ok(ResponderFile::from(named_file)),
                Err(_) => return Err(CacheInvalidationError::InvalidPath)
            }

        } else if required_space_for_new_file < 0 && size < self.size_limit {

            debug!("Cache has room for the file.");
            match InMemoryFile::open(path.as_path()) {
                Ok(file) => {
                    let arc_file: Arc<InMemoryFile> = Arc::new(file);
                    self.file_map.insert(path.clone(), arc_file.clone());
                    let cached_file = CachedFile {
                        path: path.clone(),
                        file: arc_file
                    };
                    return Ok(ResponderFile::from(cached_file))
                }
                Err(_) => {
                    return Err(CacheInvalidationError::InvalidPath)
                }
            }

        } else {
            debug!("Trying to make room for the file");

            // The access_count should have incremented since the last time this was called, so the priority must be recalculated.
            // Also, the size generally
            let new_file_access_count: usize = self.access_count_map.get(&path).unwrap_or(&1).clone();
            let new_file_priority: usize = (self.priority_function)(new_file_access_count, size);



            match self.make_room_for_new_file(required_space_for_new_file as usize, new_file_priority) {
                Ok(removed_files) => {
                    debug!("Made room for new file");
                    match InMemoryFile::open(path.as_path()) {
                        Ok(file) => {
                            let arc_file: Arc<InMemoryFile> = Arc::new(file);
                            self.file_map.insert(path.clone(), arc_file.clone());
                            let cached_file = CachedFile {
                                path,
                                file: arc_file
                            };
                            return Ok(ResponderFile::from(cached_file))
                        }
                        Err(_) => {
                            // The insertion failed, so the removed files need to be re-added to the
                            // cache
                            removed_files.into_iter().for_each( |removed_file| {
                                self.file_map.insert(removed_file.path, removed_file.file);
                            });
                            return Err(CacheInvalidationError::InvalidPath)
                        }
                    }
                }
                Err(_) => {
                    debug!("The file does not have enough priority or is too large to be accepted into the cache.");
                    // The new file would not be accepted by the cache, so instead of reading the whole file
                    // into memory, and then copying it yet again when it is attached to the body of the
                    // response, use a NamedFile instead.
                    match NamedFile::open(path) {
                        Ok(named_file) => Ok(ResponderFile::from(named_file)),
                        Err(_) => Err(CacheInvalidationError::InvalidPath)
                    }
                }
            }
        }
    }



    /// Remove the n lowest priority files to make room for a file with a size: required_space.
    ///
    /// If this returns an OK, this function has removed the required file space from the file_map.
    /// If this returns an Err, then either not enough space could be freed, or the priority of
    /// files that would need to be freed to make room for the new file is greater than the
    /// new file's priority, and as result no memory was freed.
    ///
    /// # Arguments
    ///
    /// * `required_space` - A `usize` representing the number of bytes that must be freed to make room for a new file.
    /// * `new_file_priority` - A `usize` representing the priority of the new file to be added. If the priority of the files possibly being removed
    /// is greater than this value, then the files won't be removed.
    fn make_room_for_new_file(&mut self, required_space: usize, new_file_priority: usize) -> Result<Vec<CachedFile>, CacheInvalidationError> {
        let mut possibly_freed_space: usize = 0;
        let mut priority_score_to_free: usize = 0;
        let mut file_paths_to_remove: Vec<PathBuf> = vec![];

        let mut stats: Vec<(PathBuf, FileStats)> = self.sorted_priorities();
        while possibly_freed_space < required_space {
            // pop the priority group with the lowest priority off of the vector
            match stats.pop() {
                Some(lowest) => {
                    let (lowest_key, lowest_stats) = lowest;

                    possibly_freed_space += lowest_stats.size;
                    priority_score_to_free += lowest_stats.priority;
                    file_paths_to_remove.push(lowest_key.clone());

                    // Check if total priority to free is greater than the new file's priority,
                    // If it is, then don't free the files, as they in aggregate, are more important
                    // than the new file.
                    if priority_score_to_free > new_file_priority {
                        return Err( CacheInvalidationError::NewPriorityIsNotHighEnough)
                    }
                }
                None => return Err( CacheInvalidationError::NoMoreFilesToRemove),
            };
        }

        // Hold on to the arc pointers to the files, if for whatever reason, the new file can't be
        // read, these will need to be added back to the cache.
        let mut return_vec: Vec<CachedFile> = vec![];

        // If this hasn't returned early, then the files to remove are less important than the new file.
        for file_key in file_paths_to_remove {
            // The file was accessed with this key earlier when sorting priorities.
            // Unwrapping should be safe.
            let in_memory_file = self.file_map.remove(&file_key).unwrap();
            let _ = self.file_stats_map.remove(&file_key).unwrap();

            let removed_cached_file = CachedFile {
                path: file_key.clone(),
                file: in_memory_file
            };
            return_vec.push(removed_cached_file);
        }
        return Ok(return_vec);
    }

    ///Helper function that gets the file from the cache if it exists there.
    fn get_from_cache(&mut self, path: &PathBuf) -> Option<CachedFile> {
        match self.file_map.get(path) {
            Some(in_memory_file) => {
                Some(CachedFile {
                    path: path.clone(),
                    file: in_memory_file.clone(),
                })
            }
            None => None, // File not found
        }

    }

    /// Helper function for incrementing the access count for a given file name.
    ///
    /// This should only be used in cases where the file is known to exist, to avoid bloating the access count map with useless values.
    fn increment_access_count(&mut self, path: &PathBuf) {
        let access_count: &mut usize = self.access_count_map.entry(path.to_path_buf()).or_insert(
            // By default, the count and priority will be 0.
            // The count will immediately be incremented, and the score can't be calculated without the size of the file in question.
            // Therefore, files not in the cache MUST have their priority score calculated on insertion attempt.
            0usize
        );
        *access_count += 1; // Increment the access count
    }


    /// Update the stats associated with this file.
    fn update_stats(&mut self, path: &PathBuf) {
        let size: usize = match self.file_map.get(path){
            Some(in_memory_file) => in_memory_file.size,
            None => Cache::get_file_size_from_metadata(&path).unwrap_or(0)
        };

        let access_count: usize = self.access_count_map.get(path).unwrap_or(&1).clone();

        let stats: &mut FileStats = self.file_stats_map.entry(path.to_path_buf()).or_insert(
            FileStats {
                size,
                access_count,
                priority: 0
            }
        );
        stats.size = size;
        stats.priority = (self.priority_function)(stats.access_count, stats.size); // update the priority score.
    }








    /// Gets a vector of tuples containing the Path, priority score, and size in bytes of all items
    /// in the file_map.
    ///
    /// The vector is sorted from highest to lowest priority.
    /// This allows the assumption that the last element to be popped from the vector will have the
    /// lowest priority, and therefore is the most eligible candidate for elimination from the
    /// cache.
    ///
    fn sorted_priorities(&self) -> Vec<(PathBuf, FileStats)> {

        // TODO, this simplification doesn't work yet because as this is currently called, the file_stats_map has an entry for the new file, but doesn't have an entry in the file_map. This causes an unwrap error farther down the stack. To fix, try only update after inserting.
//        let mut priorities: Vec<(PathBuf, FileStats)> = self.file_stats_map
//            .iter()
//            .map( |x| (x.0.clone(), x.1.clone()))
//            .collect();

        // TODO if the file_map and file_stats_map can be guaranteed to have the same entries, then this outer iter block for the file_map can be removed
        let mut priorities: Vec<(PathBuf, FileStats)> = self.file_map
            .iter()
            .map(|file| {
                let (file_key, _) = file;

                let stats: FileStats = self.file_stats_map
                    .get(file_key)
                    .unwrap_or(
                        &FileStats {
                            size: 0,
                            access_count: 0,
                            priority: 0,
                        }
                    )
                    .clone();

                (file_key.clone(), stats)
            })
            .collect();

        // Sort the priorities from highest priority to lowest, so when they are pop()ed later,
        // the last element will have the lowest priority.
        priorities.sort_by(|l, r| r.1.priority.cmp(&l.1.priority));
        priorities
    }



}



#[cfg(test)]
mod tests {
    extern crate test;
    extern crate tempdir;
    extern crate rand;

    use super::*;

    use self::tempdir::TempDir;
    use self::test::Bencher;
    use self::rand::{StdRng, Rng};
    use std::io::{Write, BufWriter};
    use std::fs::File;
    use rocket::response::NamedFile;
    use std::io::Read;


    const MEG1: usize = 1024 * 1024;
    const MEG2: usize = MEG1 * 2;
    const MEG5: usize = MEG1 * 5;
    const MEG10: usize = MEG1 * 10;

    const DIR_TEST: &'static str = "test1";
    const FILE_MEG1: &'static str = "meg1.txt";
    const FILE_MEG2: &'static str = "meg2.txt";
    const FILE_MEG5: &'static str = "meg5.txt";
    const FILE_MEG10: &'static str = "meg10.txt";

    // Helper function that creates test files in a directory that is cleaned up after the test runs.
    fn create_test_file(temp_dir: &TempDir, size: usize, name: &str) -> PathBuf {
        let path = temp_dir.path().join(name);
        let tmp_file = File::create(path.clone()).unwrap();
        let mut rand_data: Vec<u8> = vec![0u8; size];
        StdRng::new().unwrap().fill_bytes(rand_data.as_mut());
        let mut buffer = BufWriter::new(tmp_file);
        buffer.write(&rand_data).unwrap();
        path
    }


    // Standardize the way a file is used in these tests.
    impl ResponderFile {
        fn dummy_write(self) {
            match self {
                ResponderFile::Cached(cached_file) => {
                    let file: *const InMemoryFile = Arc::into_raw(cached_file.file);
                    unsafe {
                        let _ = (*file).bytes.clone();
                        let _ = Arc::from_raw(file); // Prevent dangling pointer?
                    }
                }
                ResponderFile::FileSystem(mut named_file) => {
                    let mut v :Vec<u8> = Vec::new();
                    let _ = named_file.read_to_end(&mut v).unwrap();
                }
            }
        }
    }

    #[bench]
    fn cache_get_10mb(b: &mut Bencher) {
        let mut cache: Cache = Cache::new(MEG1 *20); //Cache can hold 20Mb
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_10m = create_test_file(&temp_dir, MEG10, FILE_MEG10);
        cache.get(&path_10m); // add the 10 mb file to the cache

        b.iter(|| {
            let cached_file = cache.get(&path_10m).unwrap();
            cached_file.dummy_write()
        });
    }

    #[bench]
    fn cache_miss_10mb(b: &mut Bencher) {
        let mut cache: Cache = Cache::new(0);
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_10m = create_test_file(&temp_dir, MEG10, FILE_MEG10);

        b.iter(|| {
            let cached_file = cache.get(&path_10m).unwrap();
            cached_file.dummy_write()
        });
    }

    #[bench]
    fn named_file_read_10mb(b: &mut Bencher) {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_10m = create_test_file(&temp_dir, MEG10, FILE_MEG10);
        b.iter(|| {
            let mut named_file = NamedFile::open(path_10m.clone()).unwrap();
            let mut v :Vec<u8> = Vec::new();
            named_file.read_to_end(&mut v).unwrap();
        });
    }

    #[bench]
    fn cache_get_1mb(b: &mut Bencher) {
        let mut cache: Cache = Cache::new(MEG1 *20); //Cache can hold 20Mb
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);
        cache.get(&path_1m); // add the 10 mb file to the cache

        b.iter(|| {
            let cached_file = cache.get(&path_1m).unwrap();
            cached_file.dummy_write()
        });
    }

    #[bench]
    fn cache_miss_1mb(b: &mut Bencher) {
        let mut cache: Cache = Cache::new(0);
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);

        b.iter(|| {
            let cached_file = cache.get(&path_1m).unwrap();
            cached_file.dummy_write()
        });
    }

    #[bench]
    fn named_file_read_1mb(b: &mut Bencher) {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);

        b.iter(|| {
            let mut named_file = NamedFile::open(&path_1m).unwrap();
            let mut v :Vec<u8> = Vec::new();
            named_file.read_to_end(&mut v).unwrap();
        });
    }



    #[bench]
    fn cache_get_5mb(b: &mut Bencher) {
        let mut cache: Cache = Cache::new(MEG1 *20); //Cache can hold 20Mb
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);
        cache.get(&path_5m); // add the 10 mb file to the cache

        b.iter(|| {
            let cached_file = cache.get(&path_5m).unwrap();
            cached_file.dummy_write()
        });
    }

    #[bench]
    fn cache_miss_5mb(b: &mut Bencher) {
        let mut cache: Cache = Cache::new(0);
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);

        b.iter(|| {
            let cached_file = cache.get(&path_5m).unwrap();
            cached_file.dummy_write()
        });
    }

    #[bench]
    fn named_file_read_5mb(b: &mut Bencher) {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);

        b.iter(|| {
            let mut named_file = NamedFile::open(path_5m.clone()).unwrap();
            let mut v :Vec<u8> = Vec::new();
            named_file.read_to_end(&mut v).unwrap();
        });
    }

    // Constant time access regardless of size.
    #[bench]
    fn cache_get_1mb_from_1000_entry_cache(b: &mut Bencher) {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);
        let mut cache: Cache = Cache::new(MEG1 *3); //Cache can hold 3Mb
        cache.get(&path_1m); // add the file to the cache

        // Add 1024 1kib files to the cache.
        for i in 0..1024 {
            let path = create_test_file(&temp_dir, 1024, format!("{}_1kib.txt", i).as_str());
            // make sure that the file has a high priority.
            for _ in 0..10000 {
                cache.get(&path);
            }
        }

        assert_eq!(cache.used_bytes(), MEG1 * 2);

        b.iter(|| {
            let cached_file = cache.get(&path_1m).unwrap();
            cached_file.dummy_write()
        });
    }

    // There is a penalty for missing the cache.
    #[bench]
    fn cache_miss_1mb_from_1000_entry_cache(b: &mut Bencher) {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);
        let mut cache: Cache = Cache::new(MEG1 ); //Cache can hold 1Mb

        // Add 1024 1kib files to the cache.
        for i in 0..1024 {
            let path = create_test_file(&temp_dir, 1024, format!("{}_1kib.txt", i).as_str());
            // make sure that the file has a high priority.
            for _ in 0..1000 {
                cache.get(&path);
            }
        }

        b.iter(|| {
            let cached_file = cache.get(&path_1m).unwrap();
            // Mimic what is done when the response body is set.
            cached_file.dummy_write()
        });
    }

    // This is pretty much a worst-case scenario, where every file would have to be removed to make room for the new file.
    // There is a penalty for missing the cache.
    #[bench]
    fn cache_miss_5mb_from_1000_entry_cache(b: &mut Bencher) {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG1);
        let mut cache: Cache = Cache::new(MEG5 ); //Cache can hold 1Mb

        // Add 1024 5kib files to the cache.
        for i in 0..1024 {
            let path = create_test_file(&temp_dir, 1024 * 5, format!("{}_5kib.txt", i).as_str());
            // make sure that the file has a high priority by accessing it many times
            for _ in 0..1000 {
                cache.get(&path);
            }
        }

        b.iter(|| {
            let cached_file: ResponderFile = cache.get(&path_5m).unwrap();
            // Mimic what is done when the response body is set.
            cached_file.dummy_write()
        });
    }


    #[bench]
    fn in_memory_file_read_10mb(b: &mut Bencher) {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_10m = create_test_file(&temp_dir, MEG10, FILE_MEG10);

        b.iter(|| {
            let in_memory_file = Arc::new(InMemoryFile::open(path_10m.clone()).unwrap());
            let file: *const InMemoryFile = Arc::into_raw(in_memory_file);
            unsafe {
                let _ = (*file).bytes.clone();
                let _ = Arc::from_raw(file);
            }
        });
    }


    #[test]
    fn file_exceeds_size_limit() {
        let mut cache: Cache = Cache::new(MEG1 * 8); // Cache can hold only 8Mb
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_10m = create_test_file(&temp_dir, MEG10, FILE_MEG10);

        let named_file = NamedFile::open(path_10m.clone()).unwrap();

        // expect the cache to get the item from the FS.
        assert_eq!(
            cache.try_insert(path_10m),
            Ok(ResponderFile::from(named_file))
        );
    }


    #[test]
    fn file_replaces_other_file() {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();

        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);

        let named_file_1m = NamedFile::open(path_1m.clone()).unwrap();
        let named_file_1m_2 = NamedFile::open(path_1m.clone()).unwrap();

        let cached_file_5m = CachedFile::open(path_5m.clone()).unwrap();
        let cached_file_1m = CachedFile::open(path_1m.clone()).unwrap();

        let mut cache: Cache = Cache::new(5500000); //Cache can hold only 5.5Mib

        assert_eq!(
            cache.try_insert(path_5m.clone()),
            Ok(ResponderFile::from(cached_file_5m))
        );
        assert_eq!(
            cache.try_insert(path_1m.clone() ),
            Ok(ResponderFile::from(named_file_1m))
        );
        assert_eq!(
            cache.try_insert( path_1m.clone() ),
            Ok(ResponderFile::from(named_file_1m_2))
        );
        assert_eq!(
            cache.try_insert( path_1m.clone() ),
            Ok(ResponderFile::from(cached_file_1m))
        );
    }




    #[test]
    fn new_file_replaces_lowest_priority_file() {
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_1m = create_test_file(&temp_dir, MEG1, FILE_MEG1);
        let path_2m = create_test_file(&temp_dir, MEG2, FILE_MEG2);
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);


        let cached_file_5m = CachedFile::open(path_5m.clone()).unwrap();
        let cached_file_2m = CachedFile::open(path_2m.clone()).unwrap();
        let cached_file_1m = CachedFile::open(path_1m.clone()).unwrap();

        let named_file_1m = NamedFile::open(path_1m.clone()).unwrap();

        let mut cache: Cache = Cache::new(MEG1 * 7 + 2000);

        println!("1:\n{:#?}", cache);
        assert_eq!(
            cache.get(&path_5m),
            Some(ResponderFile::from(cached_file_5m))
        );

        println!("2:\n{:#?}", cache);
        assert_eq!(
            cache.get( &path_2m),
            Some(ResponderFile::from(cached_file_2m))
        );

        println!("3:\n{:#?}", cache);
        assert_eq!(
            cache.get( &path_1m ),
            Some(ResponderFile::from(named_file_1m))
        );
        println!("4:\n{:#?}", cache);
        // The cache will now accept the 1 meg file because (sqrt(2)_size * 1_access) for the old
        // file is less than (sqrt(1)_size * 2_access) for the new file.
        assert_eq!(
            cache.get(&path_1m ),
            Some(ResponderFile::from(cached_file_1m))
        );


        if let None = cache.get_from_cache(&path_1m) {
            assert_eq!(&path_1m, &PathBuf::new()) // this will fail, this comparison is just for debugging a failure.
        }

        // Get directly from the cache, no FS involved.
        if let None = cache.get_from_cache(&path_5m) {
            assert_eq!(&path_5m, &PathBuf::new()) // this will fail, this comparison is just for debugging a failure.
            // If this has failed, the cache removed the wrong file, implying the ordering of
            // priorities is wrong. It should remove the path_2m file instead.
        }

        if let Some(_) = cache.get_from_cache(&path_2m) {
            assert_eq!(&path_2m, &PathBuf::new()) // this will fail, this comparison is just for debugging a failure.
        }
    }




    #[test]
    fn remove_file() {
        let mut cache: Cache = Cache::new(MEG1 * 10);
        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);
        let path_10m: PathBuf = create_test_file(&temp_dir, MEG10, FILE_MEG10);

        let named_file: NamedFile = NamedFile::open(path_5m.clone()).unwrap();
        let cached_file: CachedFile = CachedFile::open(path_5m.clone()).unwrap();




        // expect the cache to get the item from the FS.
        assert_eq!(
            cache.get(&path_5m),
            Some(ResponderFile::from(cached_file))
        );

        cache.remove(&path_5m);

//        cache.get(path_10m.clone()); // add a bigger file to the cache
        assert!(cache.contains_key(&path_5m.clone()) == false);
    }

    #[test]
    fn refresh_file() {
        let mut cache: Cache = Cache::new(MEG1 * 10);

        let temp_dir = TempDir::new(DIR_TEST).unwrap();
        let path_5m = create_test_file(&temp_dir, MEG5, FILE_MEG5);

        let cached_file: CachedFile = CachedFile::open(path_5m.clone()).unwrap();

        assert_eq!(
            cache.get(&path_5m),
            Some(ResponderFile::from(cached_file))
        );

        assert_eq!(
            match cache.get(&path_5m).unwrap() {
                ResponderFile::Cached(c) => c.file.size,
                ResponderFile::FileSystem(_) => unreachable!()
            },
            MEG5
        );

        let path_of_file_with_10mb_but_path_name_5m = create_test_file(&temp_dir, MEG10, FILE_MEG5);
        let _cached_file_big: CachedFile = CachedFile::open(path_of_file_with_10mb_but_path_name_5m.clone() ).unwrap();

        cache.refresh(&path_5m);

        assert_eq!(
            match cache.get(&path_of_file_with_10mb_but_path_name_5m).unwrap() {
                ResponderFile::Cached(c) => c.file.size,
                ResponderFile::FileSystem(_) => unreachable!()
            },
            MEG10
        )


    }

}
