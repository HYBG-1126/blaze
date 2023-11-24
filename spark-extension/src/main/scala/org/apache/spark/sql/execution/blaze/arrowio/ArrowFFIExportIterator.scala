/*
 * Copyright 2022 The Blaze Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
package org.apache.spark.sql.execution.blaze.arrowio

import org.apache.arrow.c.ArrowArray
import org.apache.arrow.c.ArrowSchema
import org.apache.arrow.c.Data
import org.apache.arrow.vector.VectorSchemaRoot
import org.apache.arrow.vector.dictionary.DictionaryProvider.MapDictionaryProvider
import org.apache.spark.sql.catalyst.InternalRow
import org.apache.spark.sql.execution.blaze.arrowio.util.ArrowUtils
import org.apache.spark.sql.execution.blaze.arrowio.util.ArrowWriter
import org.apache.spark.sql.types.StructType
import org.apache.spark.TaskContext
import org.apache.spark.internal.Logging
import org.apache.spark.sql.blaze.BlazeConf

class ArrowFFIExportIterator(
    rowIter: Iterator[InternalRow],
    schema: StructType,
    taskContext: TaskContext,
    recordBatchSize: Int = BlazeConf.BATCH_SIZE.intConf())
    extends Iterator[(Long, Long) => Unit]
    with Logging {

  private val arrowSchema = ArrowUtils.toArrowSchema(schema)
  private val emptyDictionaryProvider = new MapDictionaryProvider()

  private var root: VectorSchemaRoot = _
  private var arrowWriter: ArrowWriter = _

  taskContext.addTaskCompletionListener[Unit](_ => close())
  taskContext.addTaskFailureListener((_, _) => close())

  override def hasNext: Boolean = {
    val hasNext = rowIter.hasNext
    if (!hasNext) {
      close()
    }
    hasNext
  }

  override def next(): (Long, Long) => Unit =
    synchronized {
      if (root == null) {
        root = VectorSchemaRoot.create(arrowSchema, ArrowUtils.rootAllocator)
        arrowWriter = ArrowWriter.create(root)
      } else {
        root.allocateNew()
        arrowWriter.reset()
      }
      var rowCount = 0

      while (rowIter.hasNext && rowCount < recordBatchSize) {
        arrowWriter.write(rowIter.next())
        rowCount += 1
      }
      arrowWriter.finish()

      (exportArrowSchemaPtr: Long, exportArrowArrayPtr: Long) => {
        Data.exportVectorSchemaRoot(
          ArrowUtils.rootAllocator,
          root,
          emptyDictionaryProvider,
          ArrowArray.wrap(exportArrowArrayPtr),
          ArrowSchema.wrap(exportArrowSchemaPtr))
        root.close()
      }
    }

  private def close(): Unit =
    synchronized {
      if (root != null) {
        root.close()
      }
    }
}
