
/// 写入数据到vec中
pub fn write<T: Copy>(vec: &mut Vec<T>, pos: usize, buf: &[T], default_value: T){
    let len = vec.len();
    let end = pos + buf.len();
    if end <= len {
        // 在长度范围内，拷贝数据
        return vec[pos..end].copy_from_slice(buf);
    }
    if pos < len {
        // 先截断vec到pos长度
        vec.truncate(pos);
    }else{
        if end > vec.capacity() {
            // 如果容量不够，则扩容
            vec.reserve(end - len);
        }
        // 在vec长度范围外，先扩大到pos位置，内容填default_value
        vec.resize(pos, default_value);
    }
    // 扩容并拷贝数据
    vec.extend_from_slice(buf);
}