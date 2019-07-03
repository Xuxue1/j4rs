/*
 * Copyright 2019 astonbitecode
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
package org.astonbitecode.j4rs.api.deploy;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.net.MalformedURLException;
import java.nio.channels.Channels;
import java.nio.channels.ReadableByteChannel;

public class FileSystemDeployer {
    private final String deployTarget;

    public FileSystemDeployer() {
        this(".");
    }

    public FileSystemDeployer(String deployTarget) {
        this.deployTarget = deployTarget;
        new File(deployTarget).mkdirs();
    }

    public void deploy(String path) throws MalformedURLException, IOException {
        File jarFile = new File(path);
        ReadableByteChannel readableByteChannel = Channels.newChannel(jarFile.toURI().toURL().openStream());
        FileOutputStream fileOutputStream = new FileOutputStream(deployTarget + File.separator + jarFile.getName());
        fileOutputStream.getChannel().transferFrom(readableByteChannel, 0, Long.MAX_VALUE);
    }
}
